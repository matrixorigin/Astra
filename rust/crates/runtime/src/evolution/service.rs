//! Evolution service — orchestrates signal collection, proposal generation, and application.

use std::sync::Arc;
use tokio::sync::Mutex;

use super::evolver;
use super::signal_collector::SignalCollector;
use super::types::{
    ApprovalStatus, EvolutionAxis, EvolutionProposal, EvolutionSignal, ToolResultContext,
    TurnSummary,
};

use crate::liquid::reflection::ReflectionEngine;
use crate::pipeline::pattern::PatternLibrary;

/// Orchestrates the evolution lifecycle: collect → propose → apply.
pub struct EvolutionService {
    collector: Mutex<SignalCollector>,
    /// Proposals generated but not yet applied (skill axis, pending user approval).
    pending_proposals: Mutex<Vec<EvolutionProposal>>,
    /// Applied proposals log (for audit/display). Bounded to last 100.
    applied_log: Mutex<Vec<EvolutionProposal>>,
    /// Optional pattern library for drift detection during flush.
    pattern_library: Option<Arc<std::sync::Mutex<PatternLibrary>>>,
    /// Cached reflection engine (stateless — reusable across calls).
    reflection_engine: ReflectionEngine,
}

impl EvolutionService {
    pub fn new() -> Self {
        Self {
            collector: Mutex::new(SignalCollector::new()),
            pending_proposals: Mutex::new(Vec::new()),
            applied_log: Mutex::new(Vec::new()),
            pattern_library: None,
            reflection_engine: ReflectionEngine::new(),
        }
    }

    /// Create with a pattern library reference for drift detection.
    pub fn with_pattern_library(mut self, lib: Arc<std::sync::Mutex<PatternLibrary>>) -> Self {
        self.pattern_library = Some(lib);
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

    /// Drain signals, generate fast-path proposals, auto-apply them,
    /// and return any that need user approval (skill axis).
    ///
    /// Also checks pattern library for drift and injects drift signals.
    ///
    /// Returns `(auto_applied, needs_approval)`.
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

        if !fast.is_empty() {
            self.apply_auto_proposals(&fast).await;
        }

        // Auto-applied proposals go to the log.
        {
            let mut log = self.applied_log.lock().await;
            for p in &fast {
                log.push(p.clone());
            }
            // Bound the log to prevent unbounded growth.
            const MAX_APPLIED_LOG: usize = 100;
            if log.len() > MAX_APPLIED_LOG {
                let excess = log.len() - MAX_APPLIED_LOG;
                log.drain(..excess);
            }
        }

        (fast, llm_signals)
    }

    async fn apply_auto_proposals(&self, proposals: &[EvolutionProposal]) {
        let Some(pattern_library) = self.pattern_library.as_ref() else {
            return;
        };
        let Ok(mut library) = pattern_library.lock() else {
            return;
        };
        for proposal in proposals {
            if let EvolutionAxis::Pattern { signature, action } = &proposal.axis {
                library.apply_evolution_action(signature, *action);
            }
        }
    }

    /// Add a skill-axis proposal (from LLM path) for user approval.
    pub async fn propose(&self, proposal: EvolutionProposal) {
        self.pending_proposals.lock().await.push(proposal);
    }

    /// Get all pending proposals awaiting user approval.
    pub async fn pending(&self) -> Vec<EvolutionProposal> {
        let mut v = self.pending_proposals.lock().await.clone();
        // Higher confidence first.
        v.sort_by(|a, b| {
            let score = |p: &EvolutionProposal| p.confidence;
            score(b)
                .partial_cmp(&score(a))
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        v
    }

    /// Approve a proposal by ID. Returns the proposal if found.
    pub async fn approve(&self, id: &str) -> Option<EvolutionProposal> {
        // Extract from pending under its own lock scope (avoid nested lock across await).
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
        // Now safe to lock applied_log without nesting.
        if let Some(p) = extracted.as_ref() {
            self.applied_log.lock().await.push(p.clone());
        }
        extracted
    }

    /// Reject a proposal by ID. Returns the proposal if found.
    pub async fn reject(&self, id: &str) -> Option<EvolutionProposal> {
        let mut pending = self.pending_proposals.lock().await;
        if let Some(pos) = pending.iter().position(|p| p.id == id) {
            let mut p = pending.remove(pos);
            p.status = ApprovalStatus::Rejected;
            Some(p)
        } else {
            None
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

    /// Parse an LLM response and queue the resulting proposals as pending.
    ///
    /// Returns the number of proposals queued.
    pub async fn ingest_reflection_response(
        &self,
        llm_response: &str,
        ctx: &crate::liquid::reflection::ReflectionContext,
    ) -> Result<usize, String> {
        let parsed = self.reflection_engine.parse_response(llm_response)?;
        let proposals = self
            .reflection_engine
            .convert_proposals(&parsed.proposals, ctx);
        let count = proposals.len();
        let mut pending = self.pending_proposals.lock().await;
        for p in proposals {
            pending.push(p);
        }
        Ok(count)
    }
}

/// Wrap in Arc for shared ownership across async tasks.
pub fn new_shared() -> Arc<EvolutionService> {
    Arc::new(EvolutionService::new())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::evolution::types::{
        ApprovalStatus, EvolutionAxis, PatternAction, SkillDiff, SkillSection,
    };
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
            historical_rate: 0.9,
            recent_rate: 0.2,
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
        let svc = EvolutionService::new();
        svc.add_signal(drift_signal("bash|read_file")).await;
        let (auto, llm) = svc.flush().await;
        assert_eq!(auto.len(), 1);
        assert_eq!(auto[0].status, ApprovalStatus::AutoApplied);
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
        let svc = EvolutionService::new();
        let proposal = EvolutionProposal {
            id: "ev_test123".into(),
            signal: tool_failure_signal("bash", Some("s")),
            axis: EvolutionAxis::Skill {
                skill_name: "review_changes".into(),
                section: SkillSection::Troubleshooting,
                diff: SkillDiff::Append {
                    content: "new rule".into(),
                },
            },
            confidence: 0.8,
            reasoning: "test".into(),
            created_at: 0,
            status: ApprovalStatus::Pending,
        };
        svc.propose(proposal).await;
        assert_eq!(svc.pending().await.len(), 1);

        let approved = svc.approve("ev_test123").await;
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
        };
        svc.propose(proposal).await;
        let rejected = svc.reject("ev_reject").await;
        assert!(rejected.is_some());
        assert_eq!(rejected.unwrap().status, ApprovalStatus::Rejected);
        assert!(svc.pending().await.is_empty());
        // Rejected proposals do NOT go to applied log
        assert!(svc.applied().await.is_empty());
    }

    #[tokio::test]
    async fn approve_nonexistent_returns_none() {
        let svc = EvolutionService::new();
        assert!(svc.approve("ev_nonexistent").await.is_none());
    }

    #[tokio::test]
    async fn reject_nonexistent_returns_none() {
        let svc = EvolutionService::new();
        assert!(svc.reject("ev_nonexistent").await.is_none());
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
        let svc = EvolutionService::new();
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
        assert_eq!(auto.len(), 2, "drift + stall → 2 auto proposals");
        assert_eq!(llm.len(), 1, "tool failure with skill → 1 LLM signal");
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
        // Need historical rate >> recent rate to trigger drift.
        // 20 successes pushes historical high, then 10 failures fills the
        // recent window (size 10) with all failures → recent_rate ≈ 0.0,
        // historical_rate ≈ 0.67, drift_score = (0.67-0.0)/0.25 = 2.68 → clamped to 1.0.
        {
            let mut l = lib.lock().unwrap();
            for _ in 0..20 {
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
    }

    #[tokio::test]
    async fn ingest_reflection_bad_json_returns_error() {
        let svc = EvolutionService::new();
        let ctx = svc.build_reflection_context("s", 1, None, 0.0, &[], vec![], vec![], None);
        let result = svc.ingest_reflection_response("not json", &ctx).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn ingest_then_approve_proposal() {
        let svc = EvolutionService::new();
        let ctx = svc.build_reflection_context("s", 1, None, 0.0, &[], vec![], vec![], None);

        let llm = r#"{"proposals": [{"axis": "pattern", "description": "Boost", "confidence": 0.9, "details": {"signature": "a", "action": "boost"}}], "summary": "ok"}"#;
        svc.ingest_reflection_response(llm, &ctx).await.unwrap();

        let pending = svc.pending().await;
        assert_eq!(pending.len(), 1);
        let id = pending[0].id.clone();

        let approved = svc.approve(&id).await;
        assert!(approved.is_some());
        assert_eq!(approved.unwrap().status, ApprovalStatus::Approved);

        // No longer pending.
        assert!(svc.pending().await.is_empty());
        // Shows in applied log.
        assert_eq!(svc.applied().await.len(), 1);
    }
}
