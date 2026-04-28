//! Evolution service — orchestrates signal collection, proposal generation, and application.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tokio::sync::Mutex;

use super::promotion_gate::{ProposalPromotionContext, evaluate_proposal_promotion};
use astra_evolution::evolver;
use astra_evolution::signal_collector::SignalCollector;
use astra_evolution::store::EvolutionStore;
use astra_evolution::types::{
    ApprovalStatus, EvolutionAxis, EvolutionProposal, EvolutionSignal, PersistedActiveCanary,
    ProposalPromotionRecommendation, ProposalPromotionVerdict, ToolResultContext, TurnSummary,
};

use crate::liquid::reflection::ReflectionEngine;
use astra_pipeline::calibration::ProgressiveCalibrator;
use astra_pipeline::pattern::PatternLibrary;

const MAX_APPLIED_LOG: usize = 100;
const MAX_RECENT_CALIBRATION_DEDUP: usize = 32;

/// Orchestrates the evolution lifecycle: collect → propose → apply.
pub struct EvolutionService {
    collector: Mutex<SignalCollector>,
    /// Proposals generated but not yet applied (or deferred behind the active
    /// canary lane).
    pending_proposals: Mutex<Vec<EvolutionProposal>>,
    /// Active canaries plus rollback snapshots. Bounded to one active canary at
    /// a time so rollback can restore an exact pre-canary snapshot.
    canary_registry: Mutex<CanaryRegistry>,
    /// Applied proposals log (for audit/display). Bounded to last 100.
    applied_log: Mutex<Vec<EvolutionProposal>>,
    /// Resolved canary outcomes (promoted / rolled back) for telemetry/history.
    resolved_canary_log: Mutex<Vec<EvolutionProposal>>,
    /// Recently processed calibration proposal identities used to suppress
    /// queue floods after identical pending/approved/rejected proposals.
    recent_calibration_dedup: Mutex<Vec<String>>,
    /// Optional pattern library for drift detection during flush.
    pattern_library: Option<Arc<std::sync::Mutex<PatternLibrary>>>,
    /// Optional progressive calibrator for calibration proposal application.
    calibrator: Option<Arc<std::sync::Mutex<ProgressiveCalibrator>>>,
    /// Optional durable store for skill evolution proposals and approved diffs.
    evolution_store: Option<Arc<EvolutionStore>>,
    /// Cached reflection engine (stateless — reusable across calls).
    reflection_engine: ReflectionEngine,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ProposalIngestOutcome {
    pub processed: usize,
    pub auto_applied: usize,
    pub canary_started: usize,
    pub queued: usize,
}

#[derive(Debug, Default)]
struct ProposalRoutingOutcome {
    auto_applied: Vec<EvolutionProposal>,
    canary_started: Vec<EvolutionProposal>,
    queued: Vec<EvolutionProposal>,
}

#[derive(Debug, Clone)]
struct CanaryExecutionSnapshot {
    pattern_library: Option<PatternLibrary>,
    calibrator: Option<ProgressiveCalibrator>,
}

#[derive(Debug, Default)]
struct CanaryRegistry {
    active: Vec<EvolutionProposal>,
    snapshots: HashMap<String, CanaryExecutionSnapshot>,
}

impl ProposalRoutingOutcome {
    fn summary(&self) -> ProposalIngestOutcome {
        ProposalIngestOutcome {
            processed: self.auto_applied.len() + self.canary_started.len() + self.queued.len(),
            auto_applied: self.auto_applied.len(),
            canary_started: self.canary_started.len(),
            queued: self.queued.len(),
        }
    }
}

impl Default for EvolutionService {
    fn default() -> Self {
        Self::new()
    }
}

impl EvolutionService {
    pub fn new() -> Self {
        Self {
            collector: Mutex::new(SignalCollector::new()),
            pending_proposals: Mutex::new(Vec::new()),
            canary_registry: Mutex::new(CanaryRegistry::default()),
            applied_log: Mutex::new(Vec::new()),
            resolved_canary_log: Mutex::new(Vec::new()),
            recent_calibration_dedup: Mutex::new(Vec::new()),
            pattern_library: None,
            calibrator: None,
            evolution_store: None,
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
        self.auto_resolve_active_canary().await;

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
        sort_proposals(self.pending_proposals.lock().await.clone())
    }

    fn annotate_promotion_verdict(
        &self,
        mut proposal: EvolutionProposal,
    ) -> Result<EvolutionProposal, String> {
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
                            promotion_signals: None,
                        },
                    )?
                } else {
                    evaluate_proposal_promotion(
                        &proposal,
                        ProposalPromotionContext {
                            pattern_library: None,
                            calibrator: None,
                            promotion_signals: None,
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
                            promotion_signals: None,
                        },
                    )?
                } else {
                    evaluate_proposal_promotion(
                        &proposal,
                        ProposalPromotionContext {
                            pattern_library: None,
                            calibrator: None,
                            promotion_signals: None,
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
                        promotion_signals: None,
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
        if let Some(candidate) = candidate {
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
                self.append_applied_log(std::slice::from_ref(p)).await;
            }
            return Ok(extracted);
        }

        self.promote_canary(id).await
    }

    /// Reject a proposal by ID. Returns the proposal if found.
    pub async fn reject(&self, id: &str) -> Result<Option<EvolutionProposal>, String> {
        let candidate = {
            let pending = self.pending_proposals.lock().await;
            pending.iter().find(|p| p.id == id).cloned()
        };
        if let Some(candidate) = candidate {
            self.persist_rejection(&candidate)?;

            let mut pending = self.pending_proposals.lock().await;
            if let Some(pos) = pending.iter().position(|p| p.id == id) {
                let mut p = pending.remove(pos);
                p.status = ApprovalStatus::Rejected;
                self.remember_recent_calibration_proposals(std::slice::from_ref(&p))
                    .await;
                return Ok(Some(p));
            }
            return Ok(None);
        }

        self.rollback_canary(id).await
    }

    /// Number of buffered signals not yet flushed.
    pub async fn signal_count(&self) -> usize {
        self.collector.lock().await.signals().len()
    }

    /// Applied proposals log.
    pub async fn applied(&self) -> Vec<EvolutionProposal> {
        self.applied_log.lock().await.clone()
    }

    /// Resolved canary outcomes log.
    pub async fn resolved_canaries(&self) -> Vec<EvolutionProposal> {
        self.resolved_canary_log.lock().await.clone()
    }

    pub async fn export_active_canary(&self) -> Option<PersistedActiveCanary> {
        let registry = self.canary_registry.lock().await;
        let proposal = registry.active.first()?.clone();
        let snapshot = registry.snapshots.get(&proposal.id)?.clone();
        Some(PersistedActiveCanary {
            proposal,
            rollback_patterns: snapshot.pattern_library.map(|library| library.export()),
            rollback_calibration: snapshot.calibrator.map(|calibrator| calibrator.export()),
        })
    }

    pub async fn restore_active_canary(
        &self,
        persisted: PersistedActiveCanary,
    ) -> Result<(), String> {
        if persisted.proposal.status != ApprovalStatus::CanaryActive {
            return Err(format!(
                "persisted canary '{}' has non-active status {:?}",
                persisted.proposal.id, persisted.proposal.status
            ));
        }

        match &persisted.proposal.axis {
            EvolutionAxis::Pattern { .. } => {
                if self.pattern_library.is_none() {
                    return Err(
                        "pattern library not configured for persisted pattern canary restore"
                            .into(),
                    );
                }
                if persisted.rollback_patterns.is_none() {
                    return Err(format!(
                        "persisted pattern canary '{}' is missing its rollback snapshot",
                        persisted.proposal.id
                    ));
                }
            }
            EvolutionAxis::Calibration { .. } => {
                if self.calibrator.is_none() {
                    return Err(
                        "progressive calibrator not configured for persisted canary restore".into(),
                    );
                }
                if persisted.rollback_calibration.is_none() {
                    return Err(format!(
                        "persisted calibration canary '{}' is missing its rollback snapshot",
                        persisted.proposal.id
                    ));
                }
            }
            EvolutionAxis::Skill { .. } | EvolutionAxis::Entity { .. } => {
                return Err(format!(
                    "persisted canary '{}' has unsupported axis for live restore",
                    persisted.proposal.id
                ));
            }
        }

        let mut registry = self.canary_registry.lock().await;
        if !registry.active.is_empty() {
            return Err("cannot restore persisted canary while another canary is active".into());
        }
        let proposal = persisted.proposal;
        let pattern_library = persisted.rollback_patterns.map(|patterns| {
            let mut library = PatternLibrary::new();
            library.overlay(&patterns);
            library
        });
        let calibrator = persisted
            .rollback_calibration
            .map(|calibration| ProgressiveCalibrator::from_export(&calibration));
        registry.snapshots.insert(
            proposal.id.clone(),
            CanaryExecutionSnapshot {
                pattern_library,
                calibrator,
            },
        );
        registry.active.push(proposal);
        Ok(())
    }

    /// Active canaries running against the live runtime state.
    pub async fn active_canaries(&self) -> Vec<EvolutionProposal> {
        let registry = self.canary_registry.lock().await;
        sort_proposals(registry.active.clone())
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
        let pending_calibration_keys: HashSet<String> = {
            let pending = self.pending_proposals.lock().await;
            pending.iter().filter_map(calibration_dedup_key).collect()
        };
        let active_calibration_keys: HashSet<String> = {
            let registry = self.canary_registry.lock().await;
            registry
                .active
                .iter()
                .filter_map(calibration_dedup_key)
                .collect()
        };
        let recent_calibration_keys: HashSet<String> = {
            let recent = self.recent_calibration_dedup.lock().await;
            recent.iter().cloned().collect()
        };
        let mut routed = ProposalRoutingOutcome::default();
        let mut batch_calibration_keys = HashSet::new();
        for proposal in proposals {
            if let Some(key) = calibration_dedup_key(&proposal)
                && (pending_calibration_keys.contains(&key)
                    || active_calibration_keys.contains(&key)
                    || recent_calibration_keys.contains(&key)
                    || !batch_calibration_keys.insert(key))
            {
                continue;
            }
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
            } else if self.should_start_canary(&proposal) {
                if let Some(started) = self.start_canary(&proposal).await {
                    routed.canary_started.push(started);
                } else {
                    routed.queued.push(proposal);
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
        drop(log);
        self.remember_recent_calibration_proposals(proposals).await;
    }

    async fn append_resolved_canary_log(&self, proposals: &[EvolutionProposal]) {
        let mut log = self.resolved_canary_log.lock().await;
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

    fn should_start_canary(&self, proposal: &EvolutionProposal) -> bool {
        proposal.promotion_verdict.as_ref().is_some_and(|verdict| {
            verdict.recommendation == ProposalPromotionRecommendation::Canary
        })
    }

    async fn remember_recent_calibration_proposals(&self, proposals: &[EvolutionProposal]) {
        let mut recent = self.recent_calibration_dedup.lock().await;
        for proposal in proposals {
            let Some(key) = calibration_dedup_key(proposal) else {
                continue;
            };
            recent.retain(|existing| existing != &key);
            recent.push(key);
        }
        if recent.len() > MAX_RECENT_CALIBRATION_DEDUP {
            let excess = recent.len() - MAX_RECENT_CALIBRATION_DEDUP;
            recent.drain(..excess);
        }
    }

    fn capture_canary_snapshot(
        &self,
        proposal: &EvolutionProposal,
    ) -> Option<CanaryExecutionSnapshot> {
        match &proposal.axis {
            EvolutionAxis::Pattern { .. } => {
                let pattern_library = self.pattern_library.as_ref()?.lock().ok()?.clone();
                Some(CanaryExecutionSnapshot {
                    pattern_library: Some(pattern_library),
                    calibrator: None,
                })
            }
            EvolutionAxis::Calibration { .. } => {
                let calibrator = self.calibrator.as_ref()?.lock().ok()?.clone();
                Some(CanaryExecutionSnapshot {
                    pattern_library: None,
                    calibrator: Some(calibrator),
                })
            }
            EvolutionAxis::Skill { .. } | EvolutionAxis::Entity { .. } => None,
        }
    }

    fn restore_canary_snapshot(&self, snapshot: &CanaryExecutionSnapshot) -> Result<(), String> {
        if let Some(pattern_library) = snapshot.pattern_library.as_ref() {
            let Some(library) = self.pattern_library.as_ref() else {
                return Err("pattern library not configured for canary rollback".into());
            };
            let Ok(mut library) = library.lock() else {
                return Err("pattern library lock poisoned during canary rollback".into());
            };
            *library = pattern_library.clone();
        }
        if let Some(calibrator) = snapshot.calibrator.as_ref() {
            let Some(active_calibrator) = self.calibrator.as_ref() else {
                return Err("progressive calibrator not configured for canary rollback".into());
            };
            let Ok(mut active_calibrator) = active_calibrator.lock() else {
                return Err("progressive calibrator lock poisoned during canary rollback".into());
            };
            *active_calibrator = calibrator.clone();
        }
        Ok(())
    }

    fn evaluate_canary_with_snapshot(
        &self,
        proposal: &EvolutionProposal,
        snapshot: &CanaryExecutionSnapshot,
    ) -> Result<ProposalPromotionVerdict, String> {
        evaluate_proposal_promotion(
            proposal,
            ProposalPromotionContext {
                pattern_library: snapshot.pattern_library.as_ref(),
                calibrator: snapshot.calibrator.as_ref(),
                promotion_signals: None,
            },
        )
    }

    async fn refresh_active_canary_verdict(
        &self,
        id: &str,
        verdict: ProposalPromotionVerdict,
    ) -> bool {
        let mut registry = self.canary_registry.lock().await;
        let Some(active) = registry
            .active
            .iter_mut()
            .find(|proposal| proposal.id == id)
        else {
            return false;
        };
        active.promotion_verdict = Some(verdict);
        true
    }

    async fn auto_resolve_active_canary(&self) {
        let Some((proposal, snapshot)) = ({
            let registry = self.canary_registry.lock().await;
            let Some(proposal) = registry.active.first().cloned() else {
                return;
            };
            let Some(snapshot) = registry.snapshots.get(&proposal.id).cloned() else {
                astra_core::agent_warn!(
                    "evolution",
                    "active canary '{}' is missing a rollback snapshot",
                    proposal.id
                );
                return;
            };
            Some((proposal, snapshot))
        }) else {
            return;
        };

        let verdict = match self.evaluate_canary_with_snapshot(&proposal, &snapshot) {
            Ok(verdict) => verdict,
            Err(err) => {
                astra_core::agent_warn!(
                    "evolution",
                    "failed to re-score canary '{}': {}",
                    proposal.id,
                    err
                );
                return;
            }
        };
        let recommendation = verdict.recommendation;
        let _ = self
            .refresh_active_canary_verdict(&proposal.id, verdict)
            .await;

        match recommendation {
            ProposalPromotionRecommendation::Promote => {
                match self.promote_canary(&proposal.id).await {
                    Ok(Some(_)) => {}
                    Ok(None) => astra_core::agent_warn!(
                        "evolution",
                        "active canary '{}' disappeared before auto-promotion",
                        proposal.id
                    ),
                    Err(err) => astra_core::agent_warn!(
                        "evolution",
                        "failed to auto-promote canary '{}': {}",
                        proposal.id,
                        err
                    ),
                }
            }
            ProposalPromotionRecommendation::Hold => match self.rollback_canary(&proposal.id).await
            {
                Ok(Some(_)) => {}
                Ok(None) => astra_core::agent_warn!(
                    "evolution",
                    "active canary '{}' disappeared before auto-rollback",
                    proposal.id
                ),
                Err(err) => astra_core::agent_warn!(
                    "evolution",
                    "failed to auto-rollback canary '{}': {}",
                    proposal.id,
                    err
                ),
            },
            ProposalPromotionRecommendation::Canary => {}
        }
    }

    async fn start_canary(&self, proposal: &EvolutionProposal) -> Option<EvolutionProposal> {
        let snapshot = self.capture_canary_snapshot(proposal)?;
        let mut registry = self.canary_registry.lock().await;
        if !registry.active.is_empty() {
            return None;
        }
        self.apply_proposal(proposal).ok()?;
        let mut started = proposal.clone();
        started.status = ApprovalStatus::CanaryActive;
        registry.snapshots.insert(started.id.clone(), snapshot);
        registry.active.push(started.clone());
        Some(started)
    }

    async fn promote_canary(&self, id: &str) -> Result<Option<EvolutionProposal>, String> {
        let promoted = {
            let mut registry = self.canary_registry.lock().await;
            let Some(pos) = registry.active.iter().position(|p| p.id == id) else {
                return Ok(None);
            };
            registry.snapshots.remove(id);
            let mut proposal = registry.active.remove(pos);
            proposal.status = ApprovalStatus::CanaryPromoted;
            proposal
        };
        self.append_applied_log(std::slice::from_ref(&promoted))
            .await;
        self.append_resolved_canary_log(std::slice::from_ref(&promoted))
            .await;
        Ok(Some(promoted))
    }

    async fn rollback_canary(&self, id: &str) -> Result<Option<EvolutionProposal>, String> {
        let rolled_back = {
            let mut registry = self.canary_registry.lock().await;
            let Some(pos) = registry.active.iter().position(|p| p.id == id) else {
                return Ok(None);
            };
            let snapshot =
                registry.snapshots.get(id).cloned().ok_or_else(|| {
                    format!("missing rollback snapshot for canary proposal '{id}'")
                })?;
            self.restore_canary_snapshot(&snapshot)?;
            registry.snapshots.remove(id);
            let mut proposal = registry.active.remove(pos);
            proposal.status = ApprovalStatus::CanaryRolledBack;
            proposal
        };
        self.remember_recent_calibration_proposals(std::slice::from_ref(&rolled_back))
            .await;
        self.append_resolved_canary_log(std::slice::from_ref(&rolled_back))
            .await;
        Ok(Some(rolled_back))
    }
}

fn pending_priority(proposal: &EvolutionProposal) -> u8 {
    proposal
        .promotion_verdict
        .as_ref()
        .map(|verdict| verdict.recommendation.priority())
        .unwrap_or(ProposalPromotionRecommendation::Hold.priority())
}

fn calibration_dedup_key(proposal: &EvolutionProposal) -> Option<String> {
    let EvolutionAxis::Calibration { axis, adjustment } = &proposal.axis else {
        return None;
    };
    Some(format!(
        "{}:{}",
        calibration_axis_identity(axis),
        calibration_adjustment_direction(*adjustment)
    ))
}

fn calibration_axis_identity(axis: &astra_evolution::types::CalibrationAxis) -> String {
    match axis {
        astra_evolution::types::CalibrationAxis::Intent(intent) => format!("intent:{intent}"),
        astra_evolution::types::CalibrationAxis::Domain(domain) => format!("domain:{domain:?}"),
        astra_evolution::types::CalibrationAxis::Task(task) => format!("task:{task:?}"),
    }
}

fn calibration_adjustment_direction(adjustment: f64) -> &'static str {
    if adjustment.is_sign_negative() {
        "negative"
    } else if adjustment > 0.0 {
        "positive"
    } else {
        "neutral"
    }
}

fn verdict_score(proposal: &EvolutionProposal) -> f64 {
    proposal
        .promotion_verdict
        .as_ref()
        .map(|verdict| verdict.overall_score)
        .unwrap_or(proposal.confidence)
}

fn sort_proposals(mut proposals: Vec<EvolutionProposal>) -> Vec<EvolutionProposal> {
    proposals.sort_by(|a, b| {
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
    proposals
}

/// Wrap in Arc for shared ownership across async tasks.
pub fn new_shared() -> Arc<EvolutionService> {
    Arc::new(EvolutionService::new())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::evolution::store::StoredStatus;
    use crate::liquid::reflection::ReflectionContext;
    use crate::pipeline::routing::{DomainHint, TaskType};
    use astra_evolution::types::{
        ApprovalStatus, CalibrationAxis, EvolutionAxis, PatternAction,
        ProposalPromotionRecommendation,
    };
    use astra_pipeline::calibration::ProgressiveCalibrator;

    fn tool_failure_signal(tool: &str, skill: Option<&str>) -> EvolutionSignal {
        EvolutionSignal::ToolFailure {
            tool_name: tool.into(),
            error_snippet: "Error: test".into(),
            failure_category: None,
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
        use astra_pipeline::pattern::PatternLibrary;

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
        // Fast path produces a calibration proposal (auto-applied or queued)
        let total = auto.len() + svc.pending().await.len();
        assert!(total >= 1, "should produce a calibration proposal");
        assert_eq!(llm.len(), 1, "with skill context → also needs LLM");
    }

    #[tokio::test]
    async fn flush_tool_failure_without_skill_not_llm() {
        let svc = EvolutionService::new();
        svc.add_signal(tool_failure_signal("bash", None)).await;
        let (auto, llm) = svc.flush().await;
        // Fast path still produces a calibration proposal
        let total = auto.len() + svc.pending().await.len();
        assert!(total >= 1, "should produce a calibration proposal");
        assert!(llm.is_empty(), "no skill context → no LLM needed");
    }

    #[tokio::test]
    async fn approve_moves_to_applied() {
        use astra_pipeline::pattern::PatternLibrary;

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
            failure_category: None,
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
        use astra_pipeline::pattern::PatternLibrary;

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
        // drift promotes; block+tool_failure fall back to canary or queue
        assert_eq!(auto.len(), 1, "drift auto-applies");
        assert_eq!(llm.len(), 1, "tool failure with skill → 1 LLM signal");
        let pending = svc.pending().await;
        let canaries = svc.active_canaries().await;
        // RepeatedStall → active canary, ToolFailure → queued calibration
        assert_eq!(pending.len(), 1);
        assert_eq!(canaries.len(), 1);
        assert_eq!(
            canaries[0]
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
    async fn calibration_queue_dedups_same_scope_while_pending() {
        let svc = EvolutionService::new();
        svc.add_signal(EvolutionSignal::ToolFailure {
            tool_name: "bash".into(),
            error_snippet: "permission denied".into(),
            failure_category: None,
            skill_context: None,
            turn_id: "t1".into(),
        })
        .await;
        let (_auto, _llm) = svc.flush().await;
        assert_eq!(svc.pending().await.len(), 1);

        svc.clear_dedup().await;
        svc.add_signal(EvolutionSignal::ToolFailure {
            tool_name: "web_fetch".into(),
            error_snippet: "timeout".into(),
            failure_category: None,
            skill_context: None,
            turn_id: "t2".into(),
        })
        .await;
        let (_auto, _llm) = svc.flush().await;
        assert_eq!(
            svc.pending().await.len(),
            1,
            "same calibration scope/sign should not enqueue duplicates while pending"
        );
    }

    #[tokio::test]
    async fn rejected_calibration_proposal_enters_recent_dedup_window() {
        let svc = EvolutionService::new();
        svc.add_signal(EvolutionSignal::UserCorrection {
            correction_text: "that's wrong".into(),
            prior_assistant_text: "draft".into(),
            skill_context: None,
            turn_id: "t1".into(),
        })
        .await;
        let (_auto, _llm) = svc.flush().await;
        let pending = svc.pending().await;
        assert_eq!(pending.len(), 1);

        let rejected = svc.reject(&pending[0].id).await.unwrap();
        assert!(rejected.is_some());
        assert!(svc.pending().await.is_empty());

        svc.clear_dedup().await;
        svc.add_signal(EvolutionSignal::UserCorrection {
            correction_text: "actually, do it this way".into(),
            prior_assistant_text: "draft".into(),
            skill_context: None,
            turn_id: "t2".into(),
        })
        .await;
        let (_auto, _llm) = svc.flush().await;
        assert!(
            svc.pending().await.is_empty(),
            "recently rejected calibration proposal should not be re-enqueued immediately"
        );
    }

    #[tokio::test]
    async fn flush_detects_drift_from_pattern_library() {
        use astra_pipeline::pattern::PatternLibrary;

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
        use astra_pipeline::pattern::PatternLibrary;

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
            failure_category: None,
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
        use astra_pipeline::pattern::PatternLibrary;

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
                canary_started: 0,
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
                canary_started: 0,
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
    async fn ingest_low_confidence_calibration_starts_canary() {
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
                canary_started: 1,
                queued: 0,
            }
        );
        assert!(svc.pending().await.is_empty());
        let canaries = svc.active_canaries().await;
        assert_eq!(canaries.len(), 1);
        assert_eq!(canaries[0].status, ApprovalStatus::CanaryActive);
        assert_eq!(
            canaries[0]
                .promotion_verdict
                .as_ref()
                .map(|v| v.recommendation),
            Some(ProposalPromotionRecommendation::Canary)
        );
        match &canaries[0].axis {
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
        assert!((threshold - 0.60).abs() < 0.01, "got {threshold}");
    }

    #[tokio::test]
    async fn approve_active_canary_promotes_it() {
        let calibrator = Arc::new(std::sync::Mutex::new(ProgressiveCalibrator::default()));
        let svc = EvolutionService::new().with_calibrator(calibrator.clone());
        let ctx = svc.build_reflection_context("s", 1, None, 0.0, &[], vec![], vec![], None);
        let llm = r#"{"proposals": [{"axis": "calibration", "description": "Nudge fetch intent threshold", "confidence": 0.80, "details": {"axis": "intent:fetch", "adjustment": 0.10}}], "summary": "ok"}"#;

        svc.ingest_reflection_response_detailed(llm, &ctx)
            .await
            .unwrap();
        let proposal_id = svc.active_canaries().await[0].id.clone();

        let approved = svc.approve(&proposal_id).await.unwrap().unwrap();
        assert_eq!(approved.status, ApprovalStatus::CanaryPromoted);
        assert!(svc.active_canaries().await.is_empty());
        assert_eq!(
            svc.applied().await[0].status,
            ApprovalStatus::CanaryPromoted
        );

        let threshold =
            calibrator
                .lock()
                .unwrap()
                .calibrated_threshold("fetch", None, TaskType::Unknown);
        assert!((threshold - 0.60).abs() < 0.01, "got {threshold}");
    }

    #[tokio::test]
    async fn reject_active_canary_rolls_back_snapshot() {
        let calibrator = Arc::new(std::sync::Mutex::new(ProgressiveCalibrator::default()));
        let svc = EvolutionService::new().with_calibrator(calibrator.clone());
        let ctx = svc.build_reflection_context("s", 1, None, 0.0, &[], vec![], vec![], None);
        let llm = r#"{"proposals": [{"axis": "calibration", "description": "Nudge fetch intent threshold", "confidence": 0.80, "details": {"axis": "intent:fetch", "adjustment": 0.10}}], "summary": "ok"}"#;

        svc.ingest_reflection_response_detailed(llm, &ctx)
            .await
            .unwrap();
        let proposal_id = svc.active_canaries().await[0].id.clone();

        let rejected = svc.reject(&proposal_id).await.unwrap().unwrap();
        assert_eq!(rejected.status, ApprovalStatus::CanaryRolledBack);
        assert!(svc.active_canaries().await.is_empty());
        assert!(svc.applied().await.is_empty());

        let threshold =
            calibrator
                .lock()
                .unwrap()
                .calibrated_threshold("fetch", None, TaskType::Unknown);
        assert!((threshold - 0.70).abs() < 0.01, "got {threshold}");
    }

    #[tokio::test]
    async fn second_canary_queues_while_one_is_active() {
        let calibrator = Arc::new(std::sync::Mutex::new(ProgressiveCalibrator::default()));
        let svc = EvolutionService::new().with_calibrator(calibrator.clone());
        let ctx = svc.build_reflection_context("s", 1, None, 0.0, &[], vec![], vec![], None);

        let first = r#"{"proposals": [{"axis": "calibration", "description": "Nudge fetch intent threshold", "confidence": 0.80, "details": {"axis": "intent:fetch", "adjustment": 0.10}}], "summary": "ok"}"#;
        let second = r#"{"proposals": [{"axis": "calibration", "description": "Nudge github domain threshold", "confidence": 0.80, "details": {"axis": "domain:github", "adjustment": 0.10}}], "summary": "ok"}"#;

        svc.ingest_reflection_response_detailed(first, &ctx)
            .await
            .unwrap();
        let outcome = svc
            .ingest_reflection_response_detailed(second, &ctx)
            .await
            .unwrap();

        assert_eq!(
            outcome,
            ProposalIngestOutcome {
                processed: 1,
                auto_applied: 0,
                canary_started: 0,
                queued: 1,
            }
        );
        assert_eq!(svc.active_canaries().await.len(), 1);
        assert_eq!(svc.pending().await.len(), 1);
        assert_eq!(
            svc.pending().await[0]
                .promotion_verdict
                .as_ref()
                .map(|v| v.recommendation),
            Some(ProposalPromotionRecommendation::Canary)
        );
    }

    #[tokio::test]
    async fn persisted_active_canary_restores_and_rolls_back_after_restart() {
        let calibrator = Arc::new(std::sync::Mutex::new(ProgressiveCalibrator::default()));
        let svc = EvolutionService::new().with_calibrator(calibrator.clone());
        let ctx = svc.build_reflection_context("s", 1, None, 0.0, &[], vec![], vec![], None);
        let llm = r#"{"proposals": [{"axis": "calibration", "description": "Nudge fetch intent threshold", "confidence": 0.80, "details": {"axis": "intent:fetch", "adjustment": 0.10}}], "summary": "ok"}"#;

        svc.ingest_reflection_response_detailed(llm, &ctx)
            .await
            .unwrap();
        let persisted = svc.export_active_canary().await.unwrap();
        assert_eq!(persisted.proposal.status, ApprovalStatus::CanaryActive);

        let restored_live_calibrator =
            Arc::new(std::sync::Mutex::new(calibrator.lock().unwrap().clone()));
        let restored = EvolutionService::new().with_calibrator(restored_live_calibrator.clone());
        restored.restore_active_canary(persisted).await.unwrap();

        let active = restored.active_canaries().await;
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].status, ApprovalStatus::CanaryActive);

        let proposal_id = active[0].id.clone();
        let rejected = restored.reject(&proposal_id).await.unwrap().unwrap();
        assert_eq!(rejected.status, ApprovalStatus::CanaryRolledBack);
        let threshold = restored_live_calibrator
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
                canary_started: 0,
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
