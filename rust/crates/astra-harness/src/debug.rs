use crate::{DecisionRecord, HarnessKernel, HookPoint, HookVerdict, RuntimeSnapshot};
use std::sync::{Arc, RwLock};

/// Breakpoint condition — when matched, the DebugKernel emits Pause.
#[derive(Debug, Clone)]
pub enum Breakpoint {
    /// Pause at a specific turn number.
    AtTurn(u32),
    /// Pause at a specific hook point.
    AtHookPoint(HookPoint),
    /// Pause when token usage exceeds threshold.
    TokenThreshold(u64),
    /// Pause when tool call count exceeds threshold.
    ToolCallThreshold(u32),
    /// Pause when context utilization exceeds fraction (0.0–1.0).
    ContextUtilizationThreshold(f32),
}

impl Breakpoint {
    fn matches(&self, record: &DecisionRecord) -> bool {
        match self {
            Self::AtTurn(turn) => record.turn == *turn,
            Self::AtHookPoint(point) => record.point == *point,
            Self::TokenThreshold(thresh) => record.snapshot.tokens_used_session >= *thresh,
            Self::ToolCallThreshold(thresh) => record.snapshot.tool_calls_this_session >= *thresh,
            Self::ContextUtilizationThreshold(thresh) => record
                .snapshot
                .context_utilization
                .is_some_and(|u| u >= *thresh),
        }
    }

    fn description(&self) -> String {
        match self {
            Self::AtTurn(t) => format!("turn == {t}"),
            Self::AtHookPoint(p) => format!("hook == {p:?}"),
            Self::TokenThreshold(t) => format!("tokens >= {t}"),
            Self::ToolCallThreshold(t) => format!("tool_calls >= {t}"),
            Self::ContextUtilizationThreshold(t) => format!("context_util >= {t:.0}%"),
        }
    }
}

/// Kernel wrapper that checks breakpoints and emits Pause verdicts.
pub struct DebugKernel {
    inner: Arc<dyn HarnessKernel>,
    breakpoints: RwLock<Vec<Breakpoint>>,
    hit_count: RwLock<u32>,
}

impl DebugKernel {
    pub fn new(inner: Arc<dyn HarnessKernel>) -> Self {
        Self {
            inner,
            breakpoints: RwLock::new(Vec::new()),
            hit_count: RwLock::new(0),
        }
    }

    pub fn add_breakpoint(&self, bp: Breakpoint) {
        if let Ok(mut bps) = self.breakpoints.write() {
            bps.push(bp);
        }
    }

    pub fn clear_breakpoints(&self) {
        if let Ok(mut bps) = self.breakpoints.write() {
            bps.clear();
        }
    }

    pub fn hit_count(&self) -> u32 {
        self.hit_count.read().ok().map(|g| *g).unwrap_or(0)
    }
}

impl HarnessKernel for DebugKernel {
    fn snapshot(&self) -> Option<RuntimeSnapshot> {
        self.inner.snapshot()
    }

    fn on_record(&self, record: &DecisionRecord) -> HookVerdict {
        // Check inner kernel first — if it blocks, breakpoints don't matter
        let inner_verdict = self.inner.on_record(record);
        if matches!(inner_verdict, HookVerdict::Block { .. }) {
            return inner_verdict;
        }

        // Check breakpoints
        if let Ok(bps) = self.breakpoints.read() {
            for bp in bps.iter() {
                if bp.matches(record) {
                    if let Ok(mut count) = self.hit_count.write() {
                        *count += 1;
                    }
                    return HookVerdict::Pause {
                        reason: format!("breakpoint hit: {}", bp.description()),
                    };
                }
            }
        }

        inner_verdict
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{InMemorySnapshotSink, SnapshotSink, StandardKernel};

    fn make_record(turn: u32, point: HookPoint, tokens: u64) -> DecisionRecord {
        DecisionRecord {
            session_id: "debug-test".into(),
            turn,
            point,
            wall_time_unix_millis: 0,
            monotonic_millis_since_session: 0,
            snapshot: RuntimeSnapshot {
                turn_number: turn,
                turns_used: turn,
                tokens_used_session: tokens,
                tool_calls_this_session: turn * 2,
                context_utilization: Some(tokens as f32 / 200_000.0),
                ..RuntimeSnapshot::empty()
            },
        }
    }

    fn make_debug_kernel() -> DebugKernel {
        let sink = InMemorySnapshotSink::arc();
        let inner = Arc::new(StandardKernel::new(
            sink as Arc<dyn SnapshotSink>,
            vec![],
        ));
        DebugKernel::new(inner)
    }

    #[test]
    fn no_breakpoints_continues() {
        let kernel = make_debug_kernel();
        let verdict = kernel.on_record(&make_record(1, HookPoint::PostTurn, 1000));
        assert!(matches!(verdict, HookVerdict::Continue));
        assert_eq!(kernel.hit_count(), 0);
    }

    #[test]
    fn breakpoint_at_turn_pauses() {
        let kernel = make_debug_kernel();
        kernel.add_breakpoint(Breakpoint::AtTurn(3));

        let v1 = kernel.on_record(&make_record(1, HookPoint::PostTurn, 1000));
        assert!(matches!(v1, HookVerdict::Continue));

        let v2 = kernel.on_record(&make_record(3, HookPoint::PostTurn, 3000));
        assert!(matches!(v2, HookVerdict::Pause { .. }));
        assert_eq!(kernel.hit_count(), 1);
    }

    #[test]
    fn breakpoint_at_hook_point_pauses() {
        let kernel = make_debug_kernel();
        kernel.add_breakpoint(Breakpoint::AtHookPoint(HookPoint::PreToolBatch));

        let v1 = kernel.on_record(&make_record(1, HookPoint::PostLlmResponse, 1000));
        assert!(matches!(v1, HookVerdict::Continue));

        let v2 = kernel.on_record(&make_record(1, HookPoint::PreToolBatch, 1000));
        assert!(matches!(v2, HookVerdict::Pause { .. }));
    }

    #[test]
    fn breakpoint_token_threshold() {
        let kernel = make_debug_kernel();
        kernel.add_breakpoint(Breakpoint::TokenThreshold(50_000));

        let v1 = kernel.on_record(&make_record(1, HookPoint::PostTurn, 10_000));
        assert!(matches!(v1, HookVerdict::Continue));

        let v2 = kernel.on_record(&make_record(2, HookPoint::PostTurn, 60_000));
        assert!(matches!(v2, HookVerdict::Pause { .. }));
    }

    #[test]
    fn breakpoint_tool_call_threshold() {
        let kernel = make_debug_kernel();
        kernel.add_breakpoint(Breakpoint::ToolCallThreshold(5));

        // turn=2 → tool_calls=4
        let v1 = kernel.on_record(&make_record(2, HookPoint::PostTurn, 1000));
        assert!(matches!(v1, HookVerdict::Continue));

        // turn=3 → tool_calls=6
        let v2 = kernel.on_record(&make_record(3, HookPoint::PostTurn, 1000));
        assert!(matches!(v2, HookVerdict::Pause { .. }));
    }

    #[test]
    fn breakpoint_context_utilization() {
        let kernel = make_debug_kernel();
        kernel.add_breakpoint(Breakpoint::ContextUtilizationThreshold(0.5));

        // tokens=50_000 → util=0.25
        let v1 = kernel.on_record(&make_record(1, HookPoint::PostTurn, 50_000));
        assert!(matches!(v1, HookVerdict::Continue));

        // tokens=120_000 → util=0.6
        let v2 = kernel.on_record(&make_record(2, HookPoint::PostTurn, 120_000));
        assert!(matches!(v2, HookVerdict::Pause { .. }));
    }

    #[test]
    fn inner_block_overrides_breakpoints() {
        let sink = InMemorySnapshotSink::arc();
        let inner = Arc::new(StandardKernel::new(
            sink as Arc<dyn SnapshotSink>,
            vec![Box::new(crate::verifiers::BudgetVerifier {
                max_turns: Some(1),
                max_tokens: None,
                max_duration_millis: None,
            })],
        ));
        let kernel = DebugKernel::new(inner);
        kernel.add_breakpoint(Breakpoint::AtTurn(2));

        let verdict = kernel.on_record(&make_record(2, HookPoint::PostTurn, 1000));
        // Inner blocks on budget (turns_used=2 > max=1), breakpoint doesn't fire
        assert!(matches!(verdict, HookVerdict::Block { .. }));
        assert_eq!(kernel.hit_count(), 0);
    }

    #[test]
    fn clear_breakpoints() {
        let kernel = make_debug_kernel();
        kernel.add_breakpoint(Breakpoint::AtTurn(1));
        kernel.clear_breakpoints();

        let verdict = kernel.on_record(&make_record(1, HookPoint::PostTurn, 1000));
        assert!(matches!(verdict, HookVerdict::Continue));
    }

    #[test]
    fn multiple_breakpoints_first_wins() {
        let kernel = make_debug_kernel();
        kernel.add_breakpoint(Breakpoint::AtTurn(1));
        kernel.add_breakpoint(Breakpoint::TokenThreshold(500));

        let verdict = kernel.on_record(&make_record(1, HookPoint::PostTurn, 1000));
        match verdict {
            HookVerdict::Pause { reason } => assert!(reason.contains("turn == 1")),
            _ => panic!("expected Pause"),
        }
        assert_eq!(kernel.hit_count(), 1);
    }
}
