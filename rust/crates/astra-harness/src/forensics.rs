use crate::trace::SessionTrace;
use crate::{HookPoint, RuntimeSnapshot};

/// Point-in-time view of session state at a specific turn.
#[derive(Debug, Clone, serde::Serialize)]
pub struct TurnContext {
    pub turn: u32,
    pub snapshot_at_end: Option<RuntimeSnapshot>,
    pub hooks_fired: Vec<HookPoint>,
    pub tokens_used: u64,
    pub tool_calls: u32,
    pub context_utilization: Option<f32>,
    pub consecutive_same_tool: u32,
    pub last_tool: Option<String>,
}

/// Session-wide forensics summary.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ForensicsSummary {
    pub session_id: String,
    pub total_turns: u32,
    pub total_records: usize,
    pub peak_context_utilization: Option<f32>,
    pub peak_consecutive_same_tool: u32,
    pub total_tokens: u64,
    pub total_tool_calls: u32,
    pub unique_hooks_used: Vec<HookPoint>,
    pub warnings: Vec<ForensicsWarning>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct ForensicsWarning {
    pub turn: u32,
    pub kind: WarningKind,
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub enum WarningKind {
    HighContextUtilization,
    ToolStallDetected,
    RapidTokenGrowth,
}

impl SessionTrace {
    /// Get the context (state) at a specific turn.
    pub fn context_at_turn(&self, turn: u32) -> Option<TurnContext> {
        let records: Vec<_> = self.records.iter().filter(|r| r.turn == turn).collect();
        if records.is_empty() {
            return None;
        }

        let last_record = records.last().unwrap();
        let hooks_fired: Vec<HookPoint> = records.iter().map(|r| r.point).collect();

        Some(TurnContext {
            turn,
            snapshot_at_end: Some(last_record.snapshot.clone()),
            hooks_fired,
            tokens_used: last_record.snapshot.tokens_used_session,
            tool_calls: last_record.snapshot.tool_calls_this_session,
            context_utilization: last_record.snapshot.context_utilization,
            consecutive_same_tool: last_record.snapshot.consecutive_same_tool,
            last_tool: last_record.snapshot.last_tool_called.clone(),
        })
    }

    /// Generate a forensics summary with automated warnings.
    pub fn forensics_summary(&self) -> ForensicsSummary {
        let mut peak_util: Option<f32> = None;
        let mut peak_stall: u32 = 0;
        let mut unique_hooks: Vec<HookPoint> = Vec::new();
        let mut warnings: Vec<ForensicsWarning> = Vec::new();
        let mut prev_tokens: u64 = 0;

        for r in &self.records {
            if !unique_hooks.contains(&r.point) {
                unique_hooks.push(r.point);
            }

            if let Some(util) = r.snapshot.context_utilization {
                if peak_util.is_none_or(|p| util > p) {
                    peak_util = Some(util);
                }
                if util > 0.9 {
                    warnings.push(ForensicsWarning {
                        turn: r.turn,
                        kind: WarningKind::HighContextUtilization,
                        message: format!("context utilization {:.0}%", util * 100.0),
                    });
                }
            }

            if r.snapshot.consecutive_same_tool > peak_stall {
                peak_stall = r.snapshot.consecutive_same_tool;
            }
            if r.snapshot.consecutive_same_tool >= 3 {
                warnings.push(ForensicsWarning {
                    turn: r.turn,
                    kind: WarningKind::ToolStallDetected,
                    message: format!(
                        "consecutive same tool: {}",
                        r.snapshot.consecutive_same_tool
                    ),
                });
            }

            let tokens = r.snapshot.tokens_used_session;
            if prev_tokens > 0 && tokens > prev_tokens * 3 {
                warnings.push(ForensicsWarning {
                    turn: r.turn,
                    kind: WarningKind::RapidTokenGrowth,
                    message: format!("tokens jumped {} → {}", prev_tokens, tokens),
                });
            }
            if tokens > 0 {
                prev_tokens = tokens;
            }
        }

        let last_snap = self.records.back().map(|r| &r.snapshot);

        ForensicsSummary {
            session_id: self.session_id.clone(),
            total_turns: self.total_turns,
            total_records: self.records.len(),
            peak_context_utilization: peak_util,
            peak_consecutive_same_tool: peak_stall,
            total_tokens: last_snap.map(|s| s.tokens_used_session).unwrap_or(0),
            total_tool_calls: last_snap.map(|s| s.tool_calls_this_session).unwrap_or(0),
            unique_hooks_used: unique_hooks,
            warnings,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{DecisionRecord, RuntimeSnapshot};

    fn make_record(turn: u32, point: HookPoint, tokens: u64, tools: u32) -> DecisionRecord {
        DecisionRecord {
            session_id: "forensics-test".into(),
            turn,
            point,
            wall_time_unix_millis: 1_000_000 + turn as u64 * 1000,
            monotonic_millis_since_session: turn as u64 * 1000,
            snapshot: RuntimeSnapshot {
                session_id: "forensics-test".into(),
                turn_number: turn,
                turns_used: turn,
                tokens_used_session: tokens,
                tool_calls_this_session: tools,
                unique_tools_used: vec!["bash".into()],
                last_tool_called: Some("bash".into()),
                ..RuntimeSnapshot::empty()
            },
        }
    }

    fn sample_trace() -> SessionTrace {
        let mut trace = SessionTrace::new("forensics-test".into());
        trace
            .records
            .push_back(make_record(0, HookPoint::SessionStart, 0, 0));
        trace
            .records
            .push_back(make_record(1, HookPoint::PreLlmRequest, 0, 0));
        trace
            .records
            .push_back(make_record(1, HookPoint::PostLlmResponse, 5_000, 0));
        trace
            .records
            .push_back(make_record(1, HookPoint::PostToolBatch, 5_000, 2));
        trace
            .records
            .push_back(make_record(1, HookPoint::PostTurn, 5_000, 2));
        trace
            .records
            .push_back(make_record(2, HookPoint::PostLlmResponse, 12_000, 2));
        trace
            .records
            .push_back(make_record(2, HookPoint::PostTurn, 12_000, 4));
        trace.total_turns = 3;
        trace
    }

    #[test]
    fn context_at_turn_returns_end_state() {
        let trace = sample_trace();
        let ctx = trace.context_at_turn(1).unwrap();
        assert_eq!(ctx.turn, 1);
        assert_eq!(ctx.tokens_used, 5_000);
        assert_eq!(ctx.tool_calls, 2);
        assert_eq!(ctx.hooks_fired.len(), 4);
    }

    #[test]
    fn context_at_turn_returns_none_for_missing() {
        let trace = sample_trace();
        assert!(trace.context_at_turn(99).is_none());
    }

    #[test]
    fn forensics_summary_basic() {
        let trace = sample_trace();
        let summary = trace.forensics_summary();
        assert_eq!(summary.session_id, "forensics-test");
        assert_eq!(summary.total_turns, 3);
        assert_eq!(summary.total_records, 7);
        assert_eq!(summary.total_tokens, 12_000);
        assert_eq!(summary.total_tool_calls, 4);
        assert!(summary.warnings.is_empty());
    }

    #[test]
    fn forensics_warns_on_high_utilization() {
        let mut trace = SessionTrace::new("s1".into());
        let mut r = make_record(1, HookPoint::PostTurn, 100_000, 5);
        r.snapshot.context_utilization = Some(0.95);
        trace.records.push_back(r);

        let summary = trace.forensics_summary();
        assert_eq!(summary.warnings.len(), 1);
        assert_eq!(
            summary.warnings[0].kind,
            WarningKind::HighContextUtilization
        );
        assert_eq!(summary.peak_context_utilization, Some(0.95));
    }

    #[test]
    fn forensics_warns_on_tool_stall() {
        let mut trace = SessionTrace::new("s1".into());
        let mut r = make_record(3, HookPoint::PostTurn, 50_000, 10);
        r.snapshot.consecutive_same_tool = 4;
        trace.records.push_back(r);

        let summary = trace.forensics_summary();
        assert!(
            summary
                .warnings
                .iter()
                .any(|w| w.kind == WarningKind::ToolStallDetected)
        );
        assert_eq!(summary.peak_consecutive_same_tool, 4);
    }

    #[test]
    fn forensics_warns_on_rapid_token_growth() {
        let mut trace = SessionTrace::new("s1".into());
        trace
            .records
            .push_back(make_record(1, HookPoint::PostTurn, 1_000, 1));
        trace
            .records
            .push_back(make_record(2, HookPoint::PostTurn, 10_000, 2));

        let summary = trace.forensics_summary();
        assert!(
            summary
                .warnings
                .iter()
                .any(|w| w.kind == WarningKind::RapidTokenGrowth)
        );
    }

    #[test]
    fn forensics_summary_serializes() {
        let trace = sample_trace();
        let summary = trace.forensics_summary();
        let json = serde_json::to_string(&summary).unwrap();
        assert!(json.contains("forensics-test"));
    }
}
