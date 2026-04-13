//! Evolution service — orchestrates signal collection, proposal generation, and application.

use std::sync::Arc;
use tokio::sync::Mutex;

use super::evolver;
use super::promotion_gate::{ProposalPromotionContext, evaluate_proposal_promotion};
use super::signal_collector::SignalCollector;
use super::store::EvolutionStore;
use super::types::{
    ApprovalStatus, EvolutionAxis, EvolutionProposal, EvolutionSignal,
    ProposalPromotionRecommendation, ToolResultContext, TurnSummary,
};

use crate::liquid::reflection::ReflectionEngine;
use crate::pipeline::calibration::ProgressiveCalibrator;
use crate::pipeline::pattern::PatternLibrary;
use crate::runtime_promotion_signals::RuntimePromotionSignals;

const MAX_APPLIED_LOG: usize = 100;

/// Orchestrates the evolution lifecycle: collect → propose → apply.
pub struct EvolutionService {
    collector: Mutex<SignalCollector>,
    /// Proposals generated but not yet applied (skill axis, pending user approval).
    pending_proposals: Mutex<Vec<EvolutionProposal>>,
    /// Applied proposals log (for audit/display). Bounded to last 100.
    applied_log: Mutex<Vec<EvolutionProposal>>,
    /// Optional pattern library for drift detection during flush.
    pattern_library: Option<Arc<std::sync::Mutex<PatternLibrary>>>,
    /// Optional progressive calibrator for calibration proposal application.
    calibrator: Option<Arc<std::sync::Mutex<ProgressiveCalibrator>>>,
    /// Optional durable store for skill evolution proposals and approved diffs.
    evolution_store: Option<Arc<EvolutionStore>>,
    /// Optional preloaded promotion signals shared across runtime promotions.
    runtime_promotion_signals: std::sync::RwLock<Option<RuntimePromotionSignals>>,
    /// Cached reflection engine (stateless — reusable across calls).
    reflection_engine: ReflectionEngine,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ProposalIngestOutcome {
    pub processed: usize,
    pub auto_applied: usize,
    pub queued: usize,
}

#[derive(Debug, Default)]
struct ProposalRoutingOutcome {
    auto_applied: Vec<EvolutionProposal>,
    queued: Vec<EvolutionProposal>,
}

impl ProposalRoutingOutcome {
    fn summary(&self) -> ProposalIngestOutcome {
        ProposalIngestOutcome {
            processed: self.auto_applied.len() + self.queued.len(),
            auto_applied: self.auto_applied.len(),
            queued: self.queued.len(),
        }
    }
}

impl EvolutionService {
    pub fn new() -> Self {
        Self {
            collector: Mutex::new(SignalCollector::new()),
            pending_proposals: Mutex::new(Vec::new()),
            applied_log: Mutex::new(Vec::new()),
            pattern_library: None,
            calibrator: None,
            evolution_store: None,
            runtime_promotion_signals: std::sync::RwLock::new(None),
            reflection_engine: ReflectionEngine::new(),
        }
    }

    /// Create with a pattern library reference for drift detection.
    pub fn with_pattern_library(mut self, lib: Arc<std::sync::Mutex<PatternLibrary>>) -> Self {
        self.pattern_library = Some(lib);
        self
    }

    /// Create with a progressive calibrator reference for calibration evolution.
    pub fn with_calibrator(
        mut self,
        calibrator: Arc<std::sync::Mutex<ProgressiveCalibrator>>,
    ) -> Self {
        self.calibrator = Some(calibrator);
        self
    }

    /// Create with a durable evolution store for skill proposals.
    pub fn with_evolution_store(mut self, store: Arc<EvolutionStore>) -> Self {
        self.evolution_store = Some(store);
        self
    }

    pub fn set_runtime_promotion_signals(&self, signals: Option<RuntimePromotionSignals>) {
        *self
            .runtime_promotion_signals
            .write()
            .unwrap_or_else(|e| e.into_inner()) = signals;
    }

    /// Feed a tool result into the signal collector.
    pub async fn on_tool_result(&self, ctx: &ToolResultContext<'_>) {
        self.collector.lock().await.on_tool_result(ctx);
    }

    /// Feed a user message into the signal collector.
    pub async fn on_user_message(
        &self,
        msg: &str,
        prior_assistant: Option<&str>,
        active_skill: Option<&str>,
        turn_id: &str,
    ) {
        self.collector
            .lock()
            .await
            .on_user_message(msg, prior_assistant, active_skill, turn_id);
    }

    /// Feed a turn-end summary into the signal collector.
    pub async fn on_turn_end(&self, summary: &TurnSummary<'_>) {
        self.collector.lock().await.on_turn_end(summary);
    }

    /// Add a pre-built signal (e.g. PatternDrift from PatternLibrary).
    pub async fn add_signal(&self, signal: EvolutionSignal) {
        self.collector.lock().await.add_signal(signal);
    }

    /// Drain signals, generate fast-path proposals, route them through auto-apply
    /// guardrails, and return any LLM-routed signals for deeper reflection.
    ///
    /// Also checks pattern library for drift and injects drift signals.
    ///
    /// Returns `(auto_applied, llm_routed_signals)`.
    pub async fn flush(&self) -> (Vec<EvolutionProposal>, Vec<EvolutionSignal>) {
        // Inject drift signals from pattern library before draining.
        if let Some(ref lib) = self.pattern_library {
            // Collect drift reports without holding the lock across await.
            let drifts = lib.lock().ok().map(|l| l.detect_drift());
            if let Some(drifts) = drifts {
                let mut collector = self.collector.lock().await;
                for d in drifts {
                    if d.is_critical {
                        collector.add_signal(EvolutionSignal::PatternDrift {
                            pattern_signature: d.signature,
                            task_type: d.task_type,
                            domain: d.domain,
                            historical_rate: d.historical_success_rate,
                            recent_rate: d.recent_success_rate,
                        });
                    }
                }
            }
        }

        let signals = self.collector.lock().await.drain();
        if signals.is_empty() {
            return (Vec::new(), Vec::new());
        }

        let fast = evolver::generate_fast_proposals(&signals);
        let llm_signals: Vec<EvolutionSignal> = signals
            .into_iter()
            .filter(|s| evolver::needs_llm(s))
            .collect();

        let auto_applied = if fast.is_empty() {
            Vec::new()
        } else {
            match self.route_proposals(fast.clone()).await {
                Ok(outcome) => outcome.auto_applied,
                Err(_) => {
                    let _ = self.enqueue_pending_proposals(fast).await;
                    Vec::new()
                }
            }
        };

        (auto_applied, llm_signals)
    }

    /// Add a skill-axis proposal (from LLM path) for user approval.
    pub async fn propose(&self, proposal: EvolutionProposal) {
        let fallback = proposal.clone();
        let proposal = self
            .annotate_promotion_verdict(proposal)
            .unwrap_or(fallback);
        self.pending_proposals.lock().await.push(proposal);
    }

    /// Get all pending proposals awaiting user approval.
    pub async fn pending(&self) -> Vec<EvolutionProposal> {
        let mut v = self.pending_proposals.lock().await.clone();
        v.sort_by(|a, b| {
            pending_priority(a)
                .cmp(&pending_priority(b))
                .then_with(|| {
                    verdict_score(b)
                        .partial_cmp(&verdict_score(a))
                        .unwrap_or(std::cmp::Ordering::Equal)
                })
                .then_with(|| {
                    b.confidence
                        .partial_cmp(&a.confidence)
                        .unwrap_or(std::cmp::Ordering::Equal)
                })
                .then_with(|| a.created_at.cmp(&b.created_at))
        });
        v
    }

    fn annotate_promotion_verdict(
        &self,
        mut proposal: EvolutionProposal,
    ) -> Result<EvolutionProposal, String> {
        let runtime_promotion_signals = self
            .runtime_promotion_signals
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let verdict = match &proposal.axis {
            EvolutionAxis::Pattern { .. } => {
                if let Some(pattern_library) = self.pattern_library.as_ref() {
                    let Ok(library) = pattern_library.lock() else {
                        return Err("pattern library lock poisoned while scoring promotion".into());
                    };
                    evaluate_proposal_promotion(
                        &proposal,
                        ProposalPromotionContext {
                            pattern_library: Some(&library),
                            calibrator: None,
                            promotion_signals: runtime_promotion_signals.as_ref(),
                        },
                    )?
                } else {
                    evaluate_proposal_promotion(
                        &proposal,
                        ProposalPromotionContext {
                            pattern_library: None,
                            calibrator: None,
                            promotion_signals: runtime_promotion_signals.as_ref(),
                        },
                    )?
                }
            }
            EvolutionAxis::Calibration { .. } => {
                if let Some(calibrator) = self.calibrator.as_ref() {
                    let Ok(calibrator) = calibrator.lock() else {
                        return Err(
                            "progressive calibrator lock poisoned while scoring promotion".into(),
                        );
                    };
                    evaluate_proposal_promotion(
                        &proposal,
                        ProposalPromotionContext {
                            pattern_library: None,
                            calibrator: Some(&calibrator),
                            promotion_signals: runtime_promotion_signals.as_ref(),
                        },
                    )?
                } else {
                    evaluate_proposal_promotion(
                        &proposal,
                        ProposalPromotionContext {
                            pattern_library: None,
                            calibrator: None,
                            promotion_signals: runtime_promotion_signals.as_ref(),
                        },
                    )?
                }
            }
            EvolutionAxis::Skill { .. } | EvolutionAxis::Entity { .. } => {
                evaluate_proposal_promotion(
                    &proposal,
                    ProposalPromotionContext {
                        pattern_library: None,
                        calibrator: None,
                        promotion_signals: runtime_promotion_signals.as_ref(),
                    },
                )?
            }
        };
        proposal.promotion_verdict = Some(verdict);
        Ok(proposal)
    }

    /// Approve a proposal by ID. Returns the proposal if found.
    pub async fn approve(&self, id: &str) -> Result<Option<EvolutionProposal>, String> {
        let candidate = {
            let pending = self.pending_proposals.lock().await;
            pending.iter().find(|p| p.id == id).cloned()
        };
        let Some(candidate) = candidate else {
            return Ok(None);
        };

        self.apply_proposal(&candidate)?;

        let extracted = {
            let mut pending = self.pending_proposals.lock().await;
            if let Some(pos) = pending.iter().position(|p| p.id == id) {
                let mut p = pending.remove(pos);
                p.status = ApprovalStatus::Approved;
                Some(p)
            } else {
                None
            }
        };
        if let Some(p) = extracted.as_ref() {
            self.applied_log.lock().await.push(p.clone());
        }
        Ok(extracted)
    }

    /// Reject a proposal by ID. Returns the proposal if found.
    pub async fn reject(&self, id: &str) -> Result<Option<EvolutionProposal>, String> {
        let candidate = {
            let pending = self.pending_proposals.lock().await;
            pending.iter().find(|p| p.id == id).cloned()
        };
        let Some(candidate) = candidate else {
            return Ok(None);
        };
        self.persist_rejection(&candidate)?;

        let mut pending = self.pending_proposals.lock().await;
        if let Some(pos) = pending.iter().position(|p| p.id == id) {
            let mut p = pending.remove(pos);
            p.status = ApprovalStatus::Rejected;
            Ok(Some(p))
        } else {
            Ok(None)
        }
    }

    /// Number of buffered signals not yet flushed.
    pub async fn signal_count(&self) -> usize {
        self.collector.lock().await.signals().len()
    }

    /// Applied proposals log.
    pub async fn applied(&self) -> Vec<EvolutionProposal> {
        self.applied_log.lock().await.clone()
    }

    /// Clear dedup keys (e.g. at conversation boundary).
    pub async fn clear_dedup(&self) {
        self.collector.lock().await.clear_dedup();
    }

    // ── Reflection integration (L2.3) ──────────────────────────────────

    /// Build a ReflectionContext from the current state + provided llm_signals.
    ///
    /// Call this after `flush_and_propose()` returns llm_signals.
    /// The caller is responsible for sending the prompt to an LLM and
    /// feeding the response back via `ingest_reflection_response()`.
    pub fn build_reflection_context(
        &self,
        session_id: &str,
        turns_completed: u32,
        scenario: Option<&str>,
        token_utilisation: f64,
        llm_signals: &[EvolutionSignal],
        tool_stats: Vec<crate::liquid::reflection::ToolStat>,
        recent_tactical_actions: Vec<String>,
        active_experiment: Option<crate::liquid::reflection::ExperimentSummary>,
    ) -> crate::liquid::reflection::ReflectionContext {
        let mut ctx = crate::liquid::reflection::ReflectionContext::new(session_id);
        ctx.turns_completed = turns_completed;
        ctx.scenario = scenario.map(String::from);
        ctx.token_utilisation = token_utilisation;
        ctx.add_signals(llm_signals);
        ctx.tool_stats = tool_stats;
        ctx.recent_tactical_actions = recent_tactical_actions;
        ctx.active_experiment = active_experiment;
        ctx
    }

    /// Build the LLM prompt pair (system, user) for a given reflection context.
    pub fn build_reflection_prompt(
        &self,
        ctx: &crate::liquid::reflection::ReflectionContext,
    ) -> (String, String) {
        self.reflection_engine.build_prompt(ctx)
    }

    /// Parse an LLM response, auto-apply eligible proposals, and queue the rest.
    pub async fn ingest_reflection_response_detailed(
        &self,
        llm_response: &str,
        ctx: &crate::liquid::reflection::ReflectionContext,
    ) -> Result<ProposalIngestOutcome, String> {
        let parsed = self.reflection_engine.parse_response(llm_response)?;
        let proposals = self
            .reflection_engine
            .convert_proposals(&parsed.proposals, ctx);
        let routed = self.route_proposals(proposals).await?;
        Ok(routed.summary())
    }

    /// Parse an LLM response and return the number of proposals processed.
    pub async fn ingest_reflection_response(
        &self,
        llm_response: &str,
        ctx: &crate::liquid::reflection::ReflectionContext,
    ) -> Result<usize, String> {
        Ok(self
            .ingest_reflection_response_detailed(llm_response, ctx)
            .await?
            .processed)
    }

    fn persist_skill_proposals(&self, proposals: &[EvolutionProposal]) -> Result<(), String> {
        let Some(store) = self.evolution_store.as_ref() else {
            return Ok(());
        };
        for proposal in proposals {
            if let EvolutionAxis::Skill { skill_name, .. } = &proposal.axis {
                store.append(skill_name, proposal)?;
            }
        }
        Ok(())
    }

    fn apply_proposal(&self, proposal: &EvolutionProposal) -> Result<(), String> {
        match &proposal.axis {
            EvolutionAxis::Skill {
                skill_name,
                section,
                diff,
            } => {
                let Some(store) = self.evolution_store.as_ref() else {
                    return Err("evolution store not configured for skill proposal approval".into());
                };
                store.apply_skill_diff(skill_name, section, diff)?;
                store
                    .mark_applied(skill_name, &proposal.id)
                    .map_err(|e| format!("skill diff applied but failed to persist approval: {e}"))
            }
            EvolutionAxis::Pattern { signature, action } => {
                let Some(pattern_library) = self.pattern_library.as_ref() else {
                    return Err(
                        "pattern library not configured for pattern proposal approval".into(),
                    );
                };
                let Ok(mut library) = pattern_library.lock() else {
                    return Err("pattern library lock poisoned during proposal approval".into());
                };
                let updated = library.apply_evolution_action(signature, *action);
                if updated == 0 {
                    return Err(format!(
                        "no patterns matched signature '{signature}' for approval"
                    ));
                }
                Ok(())
            }
            EvolutionAxis::Calibration { axis, adjustment } => {
                let Some(calibrator) = self.calibrator.as_ref() else {
                    return Err(
                        "progressive calibrator not configured for calibration proposal approval"
                            .into(),
                    );
                };
                let Ok(mut calibrator) = calibrator.lock() else {
                    return Err(
                        "progressive calibrator lock poisoned during proposal approval".into(),
                    );
                };
                calibrator
                    .apply_evolution_adjustment(axis, *adjustment)
                    .map(|_| ())
            }
            EvolutionAxis::Entity { .. } => Err("entity proposal approval is not wired yet".into()),
        }
    }

    fn persist_rejection(&self, proposal: &EvolutionProposal) -> Result<(), String> {
        match &proposal.axis {
            EvolutionAxis::Skill { skill_name, .. } => {
                let Some(store) = self.evolution_store.as_ref() else {
                    return Ok(());
                };
                store.mark_rejected(skill_name, &proposal.id)
            }
            _ => Ok(()),
        }
    }

    async fn route_proposals(
        &self,
        proposals: Vec<EvolutionProposal>,
    ) -> Result<ProposalRoutingOutcome, String> {
        let mut routed = ProposalRoutingOutcome::default();
        for proposal in proposals {
            let proposal = self.annotate_promotion_verdict(proposal)?;
            if self.should_auto_apply(&proposal) {
                match self.apply_proposal(&proposal) {
                    Ok(()) => {
                        let mut applied = proposal.clone();
                        applied.status = ApprovalStatus::AutoApplied;
                        routed.auto_applied.push(applied);
                    }
                    Err(_) => routed.queued.push(proposal),
                }
            } else {
                routed.queued.push(proposal);
            }
        }

        if !routed.queued.is_empty() {
            self.enqueue_pending_proposals(routed.queued.clone())
                .await?;
        }
        if !routed.auto_applied.is_empty() {
            self.append_applied_log(&routed.auto_applied).await;
        }
        Ok(routed)
    }

    async fn enqueue_pending_proposals(
        &self,
        proposals: Vec<EvolutionProposal>,
    ) -> Result<(), String> {
        self.persist_skill_proposals(&proposals)?;
        let mut pending = self.pending_proposals.lock().await;
        pending.extend(proposals);
        Ok(())
    }

    async fn append_applied_log(&self, proposals: &[EvolutionProposal]) {
        let mut log = self.applied_log.lock().await;
        log.extend(proposals.iter().cloned());
        if log.len() > MAX_APPLIED_LOG {
            let excess = log.len() - MAX_APPLIED_LOG;
            log.drain(..excess);
        }
    }

    fn should_auto_apply(&self, proposal: &EvolutionProposal) -> bool {
        proposal.promotion_verdict.as_ref().is_some_and(|verdict| {
            verdict.recommendation == ProposalPromotionRecommendation::Promote
        })
    }
}

fn pending_priority(proposal: &EvolutionProposal) -> u8 {
    proposal
        .promotion_verdict
        .as_ref()
        .map(|verdict| verdict.recommendation.priority())
        .unwrap_or(ProposalPromotionRecommendation::Hold.priority())
}

fn verdict_score(proposal: &EvolutionProposal) -> f64 {
    proposal
        .promotion_verdict
        .as_ref()
        .map(|verdict| verdict.overall_score)
        .unwrap_or(proposal.confidence)
}

/// Wrap in Arc for shared ownership across async tasks.
pub fn new_shared() -> Arc<EvolutionService> {
    Arc::new(EvolutionService::new())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::evolution::store::StoredStatus;
    use crate::evolution::types::{
        ApprovalStatus, CalibrationAxis, EvolutionAxis, PatternAction,
        ProposalPromotionRecommendation,
    };
    use crate::liquid::reflection::ReflectionContext;
    use crate::pipeline::calibration::ProgressiveCalibrator;
    use crate::pipeline::routing::{DomainHint, TaskType};

    fn tool_failure_signal(tool: &str, skill: Option<&str>) -> EvolutionSignal {
        EvolutionSignal::ToolFailure {
            tool_name: tool.into(),
            error_snippet: "Error: test".into(),
            skill_context: skill.map(String::from),
            turn_id: "t1".into(),
        }
    }

    fn drift_signal(sig: &str) -> EvolutionSignal {
        EvolutionSignal::PatternDrift {
            pattern_signature: sig.into(),
            task_type: TaskType::Code,
            domain: Some(DomainHint::Code),
            historical_rate: 0.95,
            recent_rate: 0.05,
        }
    }

    #[tokio::test]
    async fn flush_empty_returns_nothing() {
        let svc = EvolutionService::new();
        let (auto, llm) = svc.flush().await;
        assert!(auto.is_empty());
        assert!(llm.is_empty());
    }

    #[tokio::test]
    async fn flush_drift_auto_applies() {
        use crate::pipeline::pattern::PatternLibrary;

        let lib = Arc::new(std::sync::Mutex::new(PatternLibrary::default()));
        {
            let mut l = lib.lock().unwrap();
            l.record_outcome(
                &["bash".to_string(), "read_file".to_string()],
                TaskType::Code,
                Some(DomainHint::Code),
                true,
                0.8,
                None,
            );
        }
        let svc = EvolutionService::new().with_pattern_library(lib);
        svc.add_signal(drift_signal("bash|read_file")).await;
        let (auto, llm) = svc.flush().await;
        assert_eq!(auto.len(), 1);
        assert_eq!(auto[0].status, ApprovalStatus::AutoApplied);
        assert_eq!(
            auto[0].promotion_verdict.as_ref().map(|v| v.recommendation),
            Some(ProposalPromotionRecommendation::Promote)
        );
        assert!(llm.is_empty());
        // Should be in applied log
        assert_eq!(svc.applied().await.len(), 1);
    }

    #[tokio::test]
    async fn flush_tool_failure_with_skill_goes_to_llm() {
        let svc = EvolutionService::new();
        svc.add_signal(tool_failure_signal("bash", Some("review_changes")))
            .await;
        let (auto, llm) = svc.flush().await;
        assert!(auto.is_empty());
        assert_eq!(llm.len(), 1);
    }

    #[tokio::test]
    async fn flush_tool_failure_without_skill_dropped() {
        let svc = EvolutionService::new();
        svc.add_signal(tool_failure_signal("bash", None)).await;
        let (auto, llm) = svc.flush().await;
        assert!(auto.is_empty());
        assert!(llm.is_empty(), "no skill context → not actionable");
    }

    #[tokio::test]
    async fn approve_moves_to_applied() {
        use crate::pipeline::pattern::PatternLibrary;

        let lib = Arc::new(std::sync::Mutex::new(PatternLibrary::default()));
        {
            let mut l = lib.lock().unwrap();
            l.record_outcome(
                &["bash".to_string()],
                TaskType::Code,
                Some(DomainHint::Code),
                true,
                0.8,
                None,
            );
        }
        let svc = EvolutionService::new().with_pattern_library(lib);
        let proposal = EvolutionProposal {
            id: "ev_test123".into(),
            signal: tool_failure_signal("bash", Some("s")),
            axis: EvolutionAxis::Pattern {
                signature: "bash".into(),
                action: PatternAction::Boost,
            },
            confidence: 0.8,
            reasoning: "test".into(),
            created_at: 0,
            status: ApprovalStatus::Pending,
            promotion_verdict: None,
        };
        svc.propose(proposal).await;
        assert_eq!(svc.pending().await.len(), 1);

        let approved = svc.approve("ev_test123").await.unwrap();
        assert!(approved.is_some());
        assert_eq!(approved.unwrap().status, ApprovalStatus::Approved);
        assert!(svc.pending().await.is_empty());
        assert_eq!(svc.applied().await.len(), 1);
    }

    #[tokio::test]
    async fn reject_removes_from_pending() {
        let svc = EvolutionService::new();
        let proposal = EvolutionProposal {
            id: "ev_reject".into(),
            signal: tool_failure_signal("bash", Some("s")),
            axis: EvolutionAxis::Pattern {
                signature: "x".into(),
                action: PatternAction::Demote,
            },
            confidence: 0.5,
            reasoning: "test".into(),
            created_at: 0,
            status: ApprovalStatus::Pending,
            promotion_verdict: None,
        };
        svc.propose(proposal).await;
        let rejected = svc.reject("ev_reject").await.unwrap();
        assert!(rejected.is_some());
        assert_eq!(rejected.unwrap().status, ApprovalStatus::Rejected);
        assert!(svc.pending().await.is_empty());
        // Rejected proposals do NOT go to applied log
        assert!(svc.applied().await.is_empty());
    }

    #[tokio::test]
    async fn approve_nonexistent_returns_none() {
        let svc = EvolutionService::new();
        assert!(svc.approve("ev_nonexistent").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn reject_nonexistent_returns_none() {
        let svc = EvolutionService::new();
        assert!(svc.reject("ev_nonexistent").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn signal_count_tracks_buffered() {
        let svc = EvolutionService::new();
        assert_eq!(svc.signal_count().await, 0);
        svc.add_signal(drift_signal("a")).await;
        assert_eq!(svc.signal_count().await, 1);
        svc.flush().await;
        assert_eq!(svc.signal_count().await, 0);
    }

    #[tokio::test]
    async fn on_tool_result_collects_error() {
        let svc = EvolutionService::new();
        let ctx = ToolResultContext {
            tool_name: "bash",
            tool_args: "{}",
            result: "Error: command not found",
            is_error: true,
            duration_ms: 100,
            active_skill: Some("review_changes"),
            turn_id: "t1",
        };
        svc.on_tool_result(&ctx).await;
        assert_eq!(svc.signal_count().await, 1);
    }

    #[tokio::test]
    async fn on_user_message_detects_correction() {
        let svc = EvolutionService::new();
        svc.on_user_message("不对，应该这样", Some("I did X"), Some("skill"), "t1")
            .await;
        assert_eq!(svc.signal_count().await, 1);
    }

    #[tokio::test]
    async fn mixed_flush_separates_fast_and_llm() {
        use crate::pipeline::pattern::PatternLibrary;

        let lib = Arc::new(std::sync::Mutex::new(PatternLibrary::default()));
        {
            let mut l = lib.lock().unwrap();
            l.record_outcome(
                &["a".to_string()],
                TaskType::Code,
                Some(DomainHint::Code),
                true,
                0.8,
                None,
            );
            l.record_outcome(
                &["bash".to_string()],
                TaskType::Code,
                Some(DomainHint::Code),
                true,
                0.8,
                None,
            );
        }
        let svc = EvolutionService::new().with_pattern_library(lib);
        svc.add_signal(drift_signal("a")).await;
        svc.add_signal(tool_failure_signal("bash", Some("skill_a")))
            .await;
        svc.add_signal(EvolutionSignal::RepeatedStall {
            tool_chain: vec!["bash".into()],
            stall_count: 3,
            turn_id: "t1".into(),
        })
        .await;

        let (auto, llm) = svc.flush().await;
        assert_eq!(auto.len(), 1, "drift promotes, block falls back to canary");
        assert_eq!(llm.len(), 1, "tool failure with skill → 1 LLM signal");
        let pending = svc.pending().await;
        assert_eq!(pending.len(), 1);
        assert_eq!(
            pending[0]
                .promotion_verdict
                .as_ref()
                .map(|v| v.recommendation),
            Some(ProposalPromotionRecommendation::Canary)
        );
    }

    #[tokio::test]
    async fn clear_dedup_resets_collector() {
        let svc = EvolutionService::new();
        svc.add_signal(drift_signal("a")).await;
        svc.flush().await;
        // Same signal should be deduped
        svc.add_signal(drift_signal("a")).await;
        assert_eq!(svc.signal_count().await, 0);
        // After clear, should accept again
        svc.clear_dedup().await;
        svc.add_signal(drift_signal("a")).await;
        assert_eq!(svc.signal_count().await, 1);
    }

    #[tokio::test]
    async fn flush_detects_drift_from_pattern_library() {
        use crate::pipeline::pattern::PatternLibrary;

        let lib = Arc::new(std::sync::Mutex::new(PatternLibrary::default()));
        // Need historical rate >> recent rate to trigger drift and meet
        // auto-apply confidence threshold.
        // 100 successes pushes historical high, then 10 failures fills the
        // recent window (size 10) with all failures → recent_rate ≈ 0.0,
        // historical_rate ≈ 0.91, confidence ≈ 0.91.
        {
            let mut l = lib.lock().unwrap();
            for _ in 0..100 {
                l.record_outcome(
                    &["bash".to_string()],
                    TaskType::Code,
                    Some(DomainHint::Code),
                    true,
                    0.8,
                    None,
                );
            }
            for _ in 0..10 {
                l.record_outcome(
                    &["bash".to_string()],
                    TaskType::Code,
                    Some(DomainHint::Code),
                    false,
                    0.0,
                    None,
                );
            }
        }

        let svc = EvolutionService::new().with_pattern_library(lib);
        let (auto, _) = svc.flush().await;
        // Critical drift should produce exactly one Demote proposal.
        assert_eq!(auto.len(), 1, "expected one drift proposal");
        assert!(
            matches!(
                auto[0].axis,
                EvolutionAxis::Pattern {
                    action: PatternAction::Demote,
                    ..
                }
            ),
            "drift proposal should be Demote"
        );
        assert_eq!(auto[0].status, ApprovalStatus::AutoApplied);
        assert_eq!(
            auto[0].promotion_verdict.as_ref().map(|v| v.recommendation),
            Some(ProposalPromotionRecommendation::Promote)
        );
    }

    #[tokio::test]
    async fn flush_applies_auto_pattern_proposals_to_pattern_library() {
        use crate::pipeline::pattern::PatternLibrary;

        let lib = Arc::new(std::sync::Mutex::new(PatternLibrary::default()));
        {
            let mut l = lib.lock().unwrap();
            for _ in 0..5 {
                l.record_outcome(
                    &["bash".to_string()],
                    TaskType::Code,
                    Some(DomainHint::Code),
                    true,
                    0.8,
                    None,
                );
            }
        }
        let before = lib
            .lock()
            .unwrap()
            .pattern_stats("bash", TaskType::Code)
            .unwrap()
            .1;

        let svc = EvolutionService::new().with_pattern_library(lib.clone());
        svc.add_signal(drift_signal("bash")).await;
        let (auto, _) = svc.flush().await;

        assert_eq!(auto.len(), 1);
        let after = lib
            .lock()
            .unwrap()
            .pattern_stats("bash", TaskType::Code)
            .unwrap()
            .1;
        assert!(
            after > before,
            "auto proposal should mutate pattern library"
        );
    }

    // ── L2.3 reflection integration tests ───────────────────────────────

    #[tokio::test]
    async fn build_reflection_context_populates_fields() {
        let svc = EvolutionService::new();
        let signals = vec![EvolutionSignal::ToolFailure {
            tool_name: "bash".into(),
            error_snippet: "not found".into(),
            skill_context: Some("ops".into()),
            turn_id: "t1".into(),
        }];

        let ctx = svc.build_reflection_context(
            "test-sess",
            5,
            Some("Debugging"),
            0.42,
            &signals,
            vec![crate::liquid::reflection::ToolStat {
                tool_name: "bash".into(),
                calls: 10,
                failures: 2,
                avg_latency_ms: 150,
            }],
            vec!["IncreaseVerification".into()],
            None,
        );

        assert_eq!(ctx.session_id, "test-sess");
        assert_eq!(ctx.turns_completed, 5);
        assert_eq!(ctx.scenario.as_deref(), Some("Debugging"));
        assert!((ctx.token_utilisation - 0.42).abs() < 0.01);
        assert_eq!(ctx.signals.len(), 1);
        assert_eq!(ctx.tool_stats.len(), 1);
        assert_eq!(ctx.recent_tactical_actions, vec!["IncreaseVerification"]);
    }

    #[tokio::test]
    async fn build_reflection_prompt_produces_valid_pair() {
        let svc = EvolutionService::new();
        let ctx = svc.build_reflection_context("sess-1", 3, None, 0.1, &[], vec![], vec![], None);
        let (system, user) = svc.build_reflection_prompt(&ctx);
        assert!(system.contains("execution improvement advisor"));
        assert!(user.contains("sess-1"));
    }

    #[tokio::test]
    async fn ingest_reflection_response_queues_proposals() {
        let svc = EvolutionService::new();
        let ctx = svc.build_reflection_context(
            "sess-1",
            10,
            Some("CodeReview"),
            0.5,
            &[],
            vec![],
            vec![],
            None,
        );

        let llm_response = r#"{
            "proposals": [
                {
                    "axis": "pattern",
                    "description": "Demote failing chain",
                    "confidence": 0.8,
                    "details": { "signature": "bash→grep", "action": "demote" }
                },
                {
                    "axis": "skill",
                    "description": "Add retry hint",
                    "confidence": 0.6,
                    "details": { "skill_name": "ops", "section": "troubleshooting", "content": "retry" }
                }
            ],
            "summary": "Two issues found."
        }"#;

        let count = svc
            .ingest_reflection_response(llm_response, &ctx)
            .await
            .unwrap();
        assert_eq!(count, 2);

        // Should now appear in pending proposals.
        let pending = svc.pending().await;
        assert_eq!(pending.len(), 2);
        assert!(pending.iter().all(|p| p.id.starts_with("reflect-")));
        assert!(pending.iter().all(|p| p.promotion_verdict.is_some()));
    }

    #[tokio::test]
    async fn ingest_reflection_bad_json_returns_error() {
        let svc = EvolutionService::new();
        let ctx = svc.build_reflection_context("s", 1, None, 0.0, &[], vec![], vec![], None);
        let result = svc.ingest_reflection_response("not json", &ctx).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn ingest_high_confidence_pattern_auto_applies() {
        use crate::pipeline::pattern::PatternLibrary;

        let lib = Arc::new(std::sync::Mutex::new(PatternLibrary::default()));
        {
            let mut l = lib.lock().unwrap();
            l.record_outcome(
                &["a".to_string()],
                TaskType::Code,
                Some(DomainHint::Code),
                true,
                0.8,
                None,
            );
        }
        let svc = EvolutionService::new().with_pattern_library(lib);
        let ctx = svc.build_reflection_context("s", 1, None, 0.0, &[], vec![], vec![], None);

        let llm = r#"{"proposals": [{"axis": "pattern", "description": "Boost", "confidence": 0.9, "details": {"signature": "a", "action": "boost"}}], "summary": "ok"}"#;
        let outcome = svc
            .ingest_reflection_response_detailed(llm, &ctx)
            .await
            .unwrap();

        assert_eq!(
            outcome,
            ProposalIngestOutcome {
                processed: 1,
                auto_applied: 1,
                queued: 0,
            }
        );
        assert!(svc.pending().await.is_empty());
        let applied = svc.applied().await;
        assert_eq!(applied.len(), 1);
        assert_eq!(applied[0].status, ApprovalStatus::AutoApplied);
        assert_eq!(
            applied[0]
                .promotion_verdict
                .as_ref()
                .map(|v| v.recommendation),
            Some(ProposalPromotionRecommendation::Promote)
        );
    }

    #[tokio::test]
    async fn ingest_high_confidence_calibration_auto_applies() {
        let calibrator = Arc::new(std::sync::Mutex::new(ProgressiveCalibrator::default()));
        let svc = EvolutionService::new().with_calibrator(calibrator.clone());
        let ctx = svc.build_reflection_context("s", 1, None, 0.0, &[], vec![], vec![], None);

        let llm = r#"{"proposals": [{"axis": "calibration", "description": "Nudge fetch intent threshold", "confidence": 0.91, "details": {"axis": "intent:fetch", "adjustment": 0.10}}], "summary": "ok"}"#;
        let outcome = svc
            .ingest_reflection_response_detailed(llm, &ctx)
            .await
            .unwrap();

        assert_eq!(
            outcome,
            ProposalIngestOutcome {
                processed: 1,
                auto_applied: 1,
                queued: 0,
            }
        );
        assert!(svc.pending().await.is_empty());
        let applied = svc.applied().await;
        assert_eq!(
            applied[0]
                .promotion_verdict
                .as_ref()
                .map(|v| v.recommendation),
            Some(ProposalPromotionRecommendation::Promote)
        );
        let threshold =
            calibrator
                .lock()
                .unwrap()
                .calibrated_threshold("fetch", None, TaskType::Unknown);
        assert!((threshold - 0.60).abs() < 0.01, "got {threshold}");
    }

    #[tokio::test]
    async fn ingest_low_confidence_calibration_stays_pending() {
        let calibrator = Arc::new(std::sync::Mutex::new(ProgressiveCalibrator::default()));
        let svc = EvolutionService::new().with_calibrator(calibrator.clone());
        let ctx = svc.build_reflection_context("s", 1, None, 0.0, &[], vec![], vec![], None);

        let llm = r#"{"proposals": [{"axis": "calibration", "description": "Nudge fetch intent threshold", "confidence": 0.80, "details": {"axis": "intent:fetch", "adjustment": 0.10}}], "summary": "ok"}"#;
        let outcome = svc
            .ingest_reflection_response_detailed(llm, &ctx)
            .await
            .unwrap();

        assert_eq!(
            outcome,
            ProposalIngestOutcome {
                processed: 1,
                auto_applied: 0,
                queued: 1,
            }
        );
        let pending = svc.pending().await;
        assert_eq!(pending.len(), 1);
        assert_eq!(
            pending[0]
                .promotion_verdict
                .as_ref()
                .map(|v| v.recommendation),
            Some(ProposalPromotionRecommendation::Canary)
        );
        match &pending[0].axis {
            EvolutionAxis::Calibration {
                axis: CalibrationAxis::Intent(intent),
                adjustment,
            } => {
                assert_eq!(intent, "fetch");
                assert!((*adjustment - 0.10).abs() < 0.001);
            }
            other => panic!("expected intent calibration proposal, got {other:?}"),
        }
        let threshold =
            calibrator
                .lock()
                .unwrap()
                .calibrated_threshold("fetch", None, TaskType::Unknown);
        assert!((threshold - 0.70).abs() < 0.01, "got {threshold}");
    }

    #[tokio::test]
    async fn ingest_oversized_calibration_stays_pending() {
        let calibrator = Arc::new(std::sync::Mutex::new(ProgressiveCalibrator::default()));
        let svc = EvolutionService::new().with_calibrator(calibrator.clone());
        let ctx = svc.build_reflection_context("s", 1, None, 0.0, &[], vec![], vec![], None);

        let llm = r#"{"proposals": [{"axis": "calibration", "description": "Oversized nudge", "confidence": 0.95, "details": {"axis": "task:fetch", "adjustment": 0.25}}], "summary": "ok"}"#;
        let outcome = svc
            .ingest_reflection_response_detailed(llm, &ctx)
            .await
            .unwrap();

        assert_eq!(
            outcome,
            ProposalIngestOutcome {
                processed: 1,
                auto_applied: 0,
                queued: 1,
            }
        );
        let pending = svc.pending().await;
        assert_eq!(pending.len(), 1);
        assert_eq!(
            pending[0]
                .promotion_verdict
                .as_ref()
                .map(|v| v.recommendation),
            Some(ProposalPromotionRecommendation::Hold)
        );
        assert!(
            pending[0]
                .promotion_verdict
                .as_ref()
                .is_some_and(|v| !v.blockers.is_empty())
        );
        match &pending[0].axis {
            EvolutionAxis::Calibration {
                axis: CalibrationAxis::Task(TaskType::Fetch),
                adjustment,
            } => assert!((*adjustment - 0.25).abs() < 0.001),
            other => panic!("expected task calibration proposal, got {other:?}"),
        }
        let threshold =
            calibrator
                .lock()
                .unwrap()
                .calibrated_threshold("__auto__", None, TaskType::Fetch);
        assert!((threshold - 0.70).abs() < 0.01, "got {threshold}");
    }

    #[tokio::test]
    async fn ingest_and_approve_skill_proposal_persists_and_applies() {
        let temp = tempfile::tempdir().unwrap();
        let skill_dir = temp.path().join("review_changes");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "# Skill\n\n## Troubleshooting\n\nExisting tip.\n",
        )
        .unwrap();

        let store = Arc::new(EvolutionStore::new(temp.path().to_path_buf()));
        let svc = EvolutionService::new().with_evolution_store(store.clone());
        let ctx = ReflectionContext::new("sess-skill");
        let llm_response = r#"{
            "proposals": [
                {
                    "axis": "skill",
                    "description": "Add troubleshooting note for review_changes",
                    "confidence": 0.92,
                    "details": {
                        "skill_name": "review_changes",
                        "section": "troubleshooting",
                        "content": "Re-check staged vs unstaged diffs before final review."
                    }
                }
            ],
            "summary": "Need a sharper review troubleshooting note."
        }"#;

        let count = svc
            .ingest_reflection_response(llm_response, &ctx)
            .await
            .unwrap();
        assert_eq!(count, 1);
        assert_eq!(
            svc.pending().await[0]
                .promotion_verdict
                .as_ref()
                .map(|v| v.recommendation),
            Some(ProposalPromotionRecommendation::Hold)
        );
        let stored = store.load("review_changes").unwrap();
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].status, StoredStatus::Pending);

        let proposal_id = svc.pending().await[0].id.clone();
        let approved = svc.approve(&proposal_id).await.unwrap();
        assert!(approved.is_some());

        let updated_skill = std::fs::read_to_string(skill_dir.join("SKILL.md")).unwrap();
        assert!(updated_skill.contains("Re-check staged vs unstaged diffs before final review."));

        let stored = store.load("review_changes").unwrap();
        assert_eq!(stored[0].status, StoredStatus::Applied);
    }

    #[tokio::test]
    async fn reject_skill_proposal_marks_store_rejected() {
        let temp = tempfile::tempdir().unwrap();
        let skill_dir = temp.path().join("review_changes");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "# Skill\n\n## Troubleshooting\n\nExisting tip.\n",
        )
        .unwrap();

        let store = Arc::new(EvolutionStore::new(temp.path().to_path_buf()));
        let svc = EvolutionService::new().with_evolution_store(store.clone());
        let ctx = ReflectionContext::new("sess-skill");
        let llm_response = r#"{
            "proposals": [
                {
                    "axis": "skill",
                    "description": "Add troubleshooting note for review_changes",
                    "confidence": 0.92,
                    "details": {
                        "skill_name": "review_changes",
                        "section": "troubleshooting",
                        "content": "Do not skip diff context."
                    }
                }
            ],
            "summary": "Need a sharper review troubleshooting note."
        }"#;

        svc.ingest_reflection_response(llm_response, &ctx)
            .await
            .unwrap();
        let proposal_id = svc.pending().await[0].id.clone();
        let rejected = svc.reject(&proposal_id).await.unwrap();
        assert!(rejected.is_some());

        let stored = store.load("review_changes").unwrap();
        assert_eq!(stored[0].status, StoredStatus::Rejected);
    }
}
