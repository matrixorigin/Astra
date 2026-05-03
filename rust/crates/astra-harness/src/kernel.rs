use crate::{
    DecisionRecord, HarnessKernel, HookVerdict, RuntimeSnapshot, Severity, SnapshotSink, Verifier,
};
use std::sync::Arc;

pub struct StandardKernel {
    verifiers: Vec<Box<dyn Verifier>>,
    sink: Arc<dyn SnapshotSink>,
}

impl StandardKernel {
    pub fn new(sink: Arc<dyn SnapshotSink>, verifiers: Vec<Box<dyn Verifier>>) -> Self {
        Self { verifiers, sink }
    }

    pub fn sink(&self) -> &Arc<dyn SnapshotSink> {
        &self.sink
    }

    /// Create a kernel with default verifiers, configured from limits.
    pub fn with_default_verifiers(sink: Arc<dyn SnapshotSink>) -> Self {
        Self::configured(sink, HarnessLimits::default())
    }

    /// Create a kernel with verifiers configured from explicit limits.
    pub fn configured(sink: Arc<dyn SnapshotSink>, limits: HarnessLimits) -> Self {
        let mut verifiers: Vec<Box<dyn Verifier>> = vec![
            Box::new(crate::verifiers::BudgetVerifier {
                max_turns: limits.max_turns,
                max_tokens: limits.max_tokens,
                max_duration_millis: limits.max_duration_millis,
            }),
            Box::new(crate::verifiers::TurnGuardVerifierAdapter::default()),
            Box::new(crate::verifiers::DelegationVerifier::default()),
            Box::new(crate::verifiers::ConfidenceVerifier::default()),
        ];
        if let Some(max_cost) = limits.max_session_cost_usd {
            verifiers.push(Box::new(crate::verifiers::CostVerifier {
                prompt_cost_per_mtok: limits.prompt_cost_per_mtok.unwrap_or(3.0),
                completion_cost_per_mtok: limits.completion_cost_per_mtok.unwrap_or(15.0),
                cache_read_cost_per_mtok: limits.cache_read_cost_per_mtok.unwrap_or(0.3),
                cache_creation_cost_per_mtok: limits.cache_creation_cost_per_mtok.unwrap_or(3.75),
                max_session_cost_usd: max_cost,
            }));
        }
        if let Some(max_calls) = limits.max_tool_calls_per_session {
            verifiers.push(Box::new(crate::verifiers::ToolGuardVerifier {
                warn_tools: limits.sensitive_tools.clone(),
                max_tool_calls_per_session: Some(max_calls),
            }));
        }
        Self::new(sink, verifiers)
    }
}

/// Production limits for harness verifiers.
#[derive(Debug, Clone, Default)]
pub struct HarnessLimits {
    pub max_turns: Option<u32>,
    pub max_tokens: Option<u64>,
    pub max_duration_millis: Option<u64>,
    pub max_session_cost_usd: Option<f64>,
    pub prompt_cost_per_mtok: Option<f64>,
    pub completion_cost_per_mtok: Option<f64>,
    pub cache_read_cost_per_mtok: Option<f64>,
    pub cache_creation_cost_per_mtok: Option<f64>,
    pub max_tool_calls_per_session: Option<u32>,
    pub sensitive_tools: Vec<String>,
}

impl StandardKernel {
    pub fn verifier_count(&self) -> usize {
        self.verifiers.len()
    }
}

impl HarnessKernel for StandardKernel {
    fn snapshot(&self) -> Option<RuntimeSnapshot> {
        self.sink.latest()
    }

    fn on_record(&self, record: &DecisionRecord) -> HookVerdict {
        self.sink.update(record);

        for v in &self.verifiers {
            if !v.trigger_points().contains(&record.point) {
                continue;
            }
            let violations = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                v.check(record)
            })) {
                Ok(vs) => vs,
                Err(_) => {
                    if v.is_critical() {
                        tracing::error!(
                            verifier = v.name(),
                            "critical verifier panicked — blocking session for safety"
                        );
                        return HookVerdict::Block {
                            reason: format!(
                                "[{}] critical verifier crashed (panic); session blocked",
                                v.name()
                            ),
                        };
                    }
                    tracing::error!(
                        verifier = v.name(),
                        "verifier panicked — skipping, session continues"
                    );
                    continue;
                }
            };
            for violation in violations {
                tracing::warn!(
                    verifier = v.name(),
                    severity = ?violation.severity,
                    "harness violation: {}",
                    violation.message,
                );
                if violation.severity == Severity::Fatal {
                    return HookVerdict::Block {
                        reason: format!("[{}] {}", v.name(), violation.message),
                    };
                }
            }
        }
        HookVerdict::Continue
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{HookPoint, InMemorySnapshotSink, verifiers::*};

    fn make_record(point: HookPoint, turns: u32, streak: u32) -> DecisionRecord {
        DecisionRecord {
            session_id: "test-session".into(),
            turn: turns,
            point,
            wall_time_unix_millis: 1_000_000,
            monotonic_millis_since_session: 5_000,
            snapshot: RuntimeSnapshot {
                session_id: "test-session".into(),
                turns_used: turns,
                consecutive_same_tool: streak,
                ..RuntimeSnapshot::empty()
            },
        }
    }

    #[test]
    fn empty_verifiers_always_continue() {
        let sink = InMemorySnapshotSink::arc();
        let kernel = StandardKernel::new(sink.clone(), vec![]);
        let record = make_record(HookPoint::PostTurn, 1, 0);
        assert!(matches!(kernel.on_record(&record), HookVerdict::Continue));
        assert!(sink.latest().is_some());
    }

    #[test]
    fn budget_verifier_blocks_on_exceeded() {
        let sink = InMemorySnapshotSink::arc();
        let kernel = StandardKernel::new(
            sink.clone(),
            vec![Box::new(BudgetVerifier {
                max_turns: Some(5),
                max_tokens: None,
                max_duration_millis: None,
            })],
        );

        // Within budget
        let record = make_record(HookPoint::PostTurn, 3, 0);
        assert!(matches!(kernel.on_record(&record), HookVerdict::Continue));

        // Over budget
        let record = make_record(HookPoint::PostTurn, 6, 0);
        assert!(matches!(kernel.on_record(&record), HookVerdict::Block { .. }));
    }

    #[test]
    fn turn_guard_adapter_blocks_on_stall() {
        let sink = InMemorySnapshotSink::arc();
        let kernel = StandardKernel::new(
            sink.clone(),
            vec![Box::new(TurnGuardVerifierAdapter {
                warn_threshold: 3,
                fatal_threshold: 5,
            })],
        );

        // No stall
        let record = make_record(HookPoint::PostTurn, 1, 2);
        assert!(matches!(kernel.on_record(&record), HookVerdict::Continue));

        // Fatal stall
        let record = make_record(HookPoint::PostTurn, 1, 5);
        assert!(matches!(kernel.on_record(&record), HookVerdict::Block { .. }));
    }

    #[test]
    fn verifier_only_fires_at_trigger_points() {
        let sink = InMemorySnapshotSink::arc();
        let kernel = StandardKernel::new(
            sink.clone(),
            vec![Box::new(BudgetVerifier {
                max_turns: Some(5),
                max_tokens: None,
                max_duration_millis: None,
            })],
        );

        // BudgetVerifier triggers at PostLlmResponse and PostTurn, not SessionStart
        let record = make_record(HookPoint::SessionStart, 10, 0);
        assert!(matches!(kernel.on_record(&record), HookVerdict::Continue));

        // Same state at PostTurn → blocks
        let record = make_record(HookPoint::PostTurn, 10, 0);
        assert!(matches!(kernel.on_record(&record), HookVerdict::Block { .. }));
    }

    #[test]
    fn multiple_verifiers_first_fatal_wins() {
        let sink = InMemorySnapshotSink::arc();
        let kernel = StandardKernel::new(
            sink.clone(),
            vec![
                Box::new(BudgetVerifier {
                    max_turns: Some(5),
                    max_tokens: None,
                    max_duration_millis: None,
                }),
                Box::new(TurnGuardVerifierAdapter {
                    warn_threshold: 2,
                    fatal_threshold: 3,
                }),
            ],
        );

        // Both would fire fatal at PostTurn — first verifier wins
        let record = make_record(HookPoint::PostTurn, 10, 5);
        match kernel.on_record(&record) {
            HookVerdict::Block { reason } => {
                assert!(reason.contains("[budget]"));
            }
            _ => panic!("expected block"),
        }
    }

    #[test]
    fn sink_updates_on_every_record() {
        let sink = InMemorySnapshotSink::arc();
        let kernel = StandardKernel::new(sink.clone(), vec![]);

        assert!(sink.latest().is_none());

        let record = make_record(HookPoint::SessionStart, 0, 0);
        kernel.on_record(&record);
        let snap = sink.latest().unwrap();
        assert_eq!(snap.turns_used, 0);

        let record = make_record(HookPoint::PostTurn, 3, 0);
        kernel.on_record(&record);
        let snap = sink.latest().unwrap();
        assert_eq!(snap.turns_used, 3);
    }

    #[test]
    fn snapshot_returns_latest() {
        let sink = InMemorySnapshotSink::arc();
        let kernel = StandardKernel::new(sink.clone(), vec![]);

        assert!(kernel.snapshot().is_none());

        let record = make_record(HookPoint::PostTurn, 5, 0);
        kernel.on_record(&record);

        let snap = kernel.snapshot().unwrap();
        assert_eq!(snap.turns_used, 5);
        assert_eq!(snap.session_id, "test-session");
    }

    // ── Panicking verifier recovery ─────────────────────────────────────

    struct PanickingVerifier;

    impl crate::Verifier for PanickingVerifier {
        fn name(&self) -> &'static str {
            "panicker"
        }
        fn trigger_points(&self) -> &'static [HookPoint] {
            &[HookPoint::PostTurn]
        }
        fn check(&self, _record: &DecisionRecord) -> Vec<crate::Violation> {
            panic!("intentional verifier panic");
        }
    }

    #[test]
    fn panicking_verifier_does_not_crash_kernel() {
        let sink = InMemorySnapshotSink::arc();
        let kernel = StandardKernel::new(
            sink.clone(),
            vec![Box::new(PanickingVerifier)],
        );

        // Should not panic — catch_unwind should absorb it
        let verdict = kernel.on_record(&make_record(HookPoint::PostTurn, 1, 0));
        assert!(matches!(verdict, HookVerdict::Continue));
        // Sink should still have been updated
        assert!(sink.latest().is_some());
    }

    #[test]
    fn panicking_verifier_doesnt_block_other_verifiers() {
        let sink = InMemorySnapshotSink::arc();
        let kernel = StandardKernel::new(
            sink.clone(),
            vec![
                Box::new(PanickingVerifier),
                Box::new(BudgetVerifier {
                    max_turns: Some(1),
                    max_tokens: None,
                    max_duration_millis: None,
                }),
            ],
        );

        // Panicker fires first (PostTurn), gets caught.
        // BudgetVerifier fires second, sees turns_used=5 > 1 → Fatal → Block
        let record = make_record(HookPoint::PostTurn, 5, 0);
        let verdict = kernel.on_record(&record);
        assert!(matches!(verdict, HookVerdict::Block { .. }));
    }

    // ── End-to-end: full verifier → Block path ──────────────────────────

    #[test]
    fn e2e_budget_block_verdict_returned() {
        let sink = InMemorySnapshotSink::arc();
        let kernel = StandardKernel::new(
            sink.clone() as Arc<dyn crate::SnapshotSink>,
            vec![
                Box::new(BudgetVerifier {
                    max_turns: Some(10),
                    max_tokens: Some(100_000),
                    max_duration_millis: None,
                }),
                Box::new(TurnGuardVerifierAdapter::default()),
            ],
        );

        // Normal turn: within budget → Continue
        let record = make_record(HookPoint::PostTurn, 5, 0);
        assert!(matches!(kernel.on_record(&record), HookVerdict::Continue));

        // Exceeds turn budget → Block
        let record = make_record(HookPoint::PostTurn, 11, 0);
        match kernel.on_record(&record) {
            HookVerdict::Block { reason } => {
                assert!(reason.contains("budget"));
                assert!(reason.contains("turn"));
            }
            _ => panic!("expected Block verdict"),
        }

        // Exceeds stall threshold → Block
        let mut record = make_record(HookPoint::PostTurn, 3, 6);
        record.snapshot.consecutive_same_tool = 6;
        match kernel.on_record(&record) {
            HookVerdict::Block { reason } => {
                assert!(reason.contains("turn_guard"));
            }
            _ => panic!("expected Block from stall"),
        }
    }

    // ── Critical verifier panic → Block ─────────────────────────────────

    struct CriticalPanickingVerifier;

    impl crate::Verifier for CriticalPanickingVerifier {
        fn name(&self) -> &'static str {
            "critical_panicker"
        }
        fn trigger_points(&self) -> &'static [HookPoint] {
            &[HookPoint::PostTurn]
        }
        fn is_critical(&self) -> bool {
            true
        }
        fn check(&self, _record: &DecisionRecord) -> Vec<crate::Violation> {
            panic!("critical verifier crash");
        }
    }

    #[test]
    fn critical_panicking_verifier_blocks_session() {
        let sink = InMemorySnapshotSink::arc();
        let kernel = StandardKernel::new(sink.clone(), vec![Box::new(CriticalPanickingVerifier)]);

        let verdict = kernel.on_record(&make_record(HookPoint::PostTurn, 1, 0));
        match verdict {
            HookVerdict::Block { reason } => {
                assert!(reason.contains("critical_panicker"));
                assert!(reason.contains("crashed"));
            }
            _ => panic!("expected Block from critical verifier panic"),
        }
    }

    #[test]
    fn non_critical_panicking_verifier_continues() {
        let sink = InMemorySnapshotSink::arc();
        let kernel = StandardKernel::new(sink.clone(), vec![Box::new(PanickingVerifier)]);

        let verdict = kernel.on_record(&make_record(HookPoint::PostTurn, 1, 0));
        assert!(matches!(verdict, HookVerdict::Continue));
    }
}
