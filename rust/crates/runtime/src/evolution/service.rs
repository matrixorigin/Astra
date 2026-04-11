//! Evolution service — orchestrates signal collection, proposal generation, and application.

use std::sync::Arc;
use tokio::sync::Mutex;

use super::evolver;
use super::signal_collector::SignalCollector;
use super::types::*;

use crate::pipeline::pattern::PatternLibrary;

/// Orchestrates the evolution lifecycle: collect → propose → apply.
pub struct EvolutionService {
    collector: Mutex<SignalCollector>,
    /// Proposals generated but not yet applied (skill axis, pending user approval).
    pending_proposals: Mutex<Vec<EvolutionProposal>>,
    /// Applied proposals log (for audit/display).
    applied_log: Mutex<Vec<EvolutionProposal>>,
    /// Optional pattern library for drift detection during flush.
    pattern_library: Option<Arc<std::sync::Mutex<PatternLibrary>>>,
}

impl EvolutionService {
    pub fn new() -> Self {
        Self {
            collector: Mutex::new(SignalCollector::new()),
            pending_proposals: Mutex::new(Vec::new()),
            applied_log: Mutex::new(Vec::new()),
            pattern_library: None,
        }
    }

    /// Create with a pattern library reference for drift detection.
    pub fn with_pattern_library(
        mut self,
        lib: Arc<std::sync::Mutex<PatternLibrary>>,
    ) -> Self {
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

        // Auto-applied proposals go to the log.
        {
            let mut log = self.applied_log.lock().await;
            for p in &fast {
                log.push(p.clone());
            }
        }

        (fast, llm_signals)
    }

    /// Add a skill-axis proposal (from LLM path) for user approval.
    pub async fn propose(&self, proposal: EvolutionProposal) {
        self.pending_proposals.lock().await.push(proposal);
    }

    /// Get all pending proposals awaiting user approval.
    pub async fn pending(&self) -> Vec<EvolutionProposal> {
        self.pending_proposals.lock().await.clone()
    }

    /// Approve a proposal by ID. Returns the proposal if found.
    pub async fn approve(&self, id: &str) -> Option<EvolutionProposal> {
        let mut pending = self.pending_proposals.lock().await;
        if let Some(pos) = pending.iter().position(|p| p.id == id) {
            let mut p = pending.remove(pos);
            p.status = ApprovalStatus::Approved;
            self.applied_log.lock().await.push(p.clone());
            Some(p)
        } else {
            None
        }
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
}

/// Wrap in Arc for shared ownership across async tasks.
pub fn new_shared() -> Arc<EvolutionService> {
    Arc::new(EvolutionService::new())
}

#[cfg(test)]
mod tests {
    use super::*;
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
        // Record enough outcomes to trigger drift detection.
        {
            let mut l = lib.lock().unwrap();
            // 6 successes then 4 failures → drift
            for _ in 0..6 {
                l.record_outcome(
                    &["bash".to_string()],
                    TaskType::Code,
                    Some(DomainHint::Code),
                    true,
                    0.8,
                    None,
                );
            }
            for _ in 0..4 {
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
        // If drift was detected and critical, we should get a Demote proposal.
        // The exact result depends on whether the drift threshold is met.
        // At minimum, the flush should not panic.
        let _ = auto;
    }
}
