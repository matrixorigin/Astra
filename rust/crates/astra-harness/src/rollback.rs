use crate::trace::SessionTrace;
use crate::HookPoint;

/// Assessment of what would need to be reversed to rollback to a target turn.
#[derive(Debug, Clone, serde::Serialize)]
pub struct RollbackAssessment {
    pub target_turn: u32,
    pub current_turn: u32,
    pub turns_to_reverse: u32,
    pub tool_calls_to_reverse: u32,
    pub tokens_to_reclaim: u64,
    pub tools_used_after_target: Vec<String>,
    pub feasibility: RollbackFeasibility,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub enum RollbackFeasibility {
    /// Rollback is straightforward (only reads after target).
    Safe,
    /// Rollback has side effects that may be hard to undo.
    HasSideEffects,
    /// Target turn doesn't exist in the trace.
    InvalidTarget,
}

impl SessionTrace {
    pub fn assess_rollback(&self, target_turn: u32) -> RollbackAssessment {
        let current_turn = self
            .records
            .back()
            .map(|r| r.turn)
            .unwrap_or(0);

        if target_turn > current_turn || self.records.is_empty() {
            return RollbackAssessment {
                target_turn,
                current_turn,
                turns_to_reverse: 0,
                tool_calls_to_reverse: 0,
                tokens_to_reclaim: 0,
                tools_used_after_target: vec![],
                feasibility: RollbackFeasibility::InvalidTarget,
            };
        }

        let target_snap = self
            .records
            .iter()
            .rfind(|r| r.turn == target_turn);
        let current_snap = self.records.back();

        let (target_tokens, target_tools) = target_snap
            .map(|r| (r.snapshot.tokens_used_session, r.snapshot.tool_calls_this_session))
            .unwrap_or((0, 0));
        let (current_tokens, current_tools) = current_snap
            .map(|r| (r.snapshot.tokens_used_session, r.snapshot.tool_calls_this_session))
            .unwrap_or((0, 0));

        let mut tools_after: Vec<String> = Vec::new();
        for r in &self.records {
            if r.turn > target_turn
                && (r.point == HookPoint::PostToolBatch || r.point == HookPoint::PostTurn)
            {
                for tool in &r.snapshot.unique_tools_used {
                    if !tools_after.contains(tool) {
                        tools_after.push(tool.clone());
                    }
                }
            }
        }

        let has_mutating_tools = tools_after.iter().any(|t| is_mutating_tool(t));

        RollbackAssessment {
            target_turn,
            current_turn,
            turns_to_reverse: current_turn.saturating_sub(target_turn),
            tool_calls_to_reverse: current_tools.saturating_sub(target_tools),
            tokens_to_reclaim: current_tokens.saturating_sub(target_tokens),
            tools_used_after_target: tools_after,
            feasibility: if has_mutating_tools {
                RollbackFeasibility::HasSideEffects
            } else {
                RollbackFeasibility::Safe
            },
        }
    }
}

fn is_mutating_tool(name: &str) -> bool {
    matches!(
        name,
        "bash"
            | "write_file"
            | "edit_file"
            | "execute_sql"
            | "delete_file"
            | "create_directory"
            | "git_commit"
            | "execute"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{DecisionRecord, RuntimeSnapshot};

    fn make_record(
        turn: u32,
        point: HookPoint,
        tokens: u64,
        tools: u32,
        tool_names: &[&str],
    ) -> DecisionRecord {
        DecisionRecord {
            session_id: "rollback-test".into(),
            turn,
            point,
            wall_time_unix_millis: 0,
            monotonic_millis_since_session: 0,
            snapshot: RuntimeSnapshot {
                turn_number: turn,
                turns_used: turn,
                tokens_used_session: tokens,
                tool_calls_this_session: tools,
                unique_tools_used: tool_names.iter().map(|s| s.to_string()).collect(),
                ..RuntimeSnapshot::empty()
            },
        }
    }

    fn sample_trace() -> SessionTrace {
        let mut trace = SessionTrace::new("rollback-test".into());
        trace.records.push_back(make_record(0, HookPoint::SessionStart, 0, 0, &[]));
        trace.records.push_back(make_record(
            1,
            HookPoint::PostToolBatch,
            5_000,
            2,
            &["read_file"],
        ));
        trace.records.push_back(make_record(1, HookPoint::PostTurn, 5_000, 2, &["read_file"]));
        trace.records.push_back(make_record(
            2,
            HookPoint::PostToolBatch,
            12_000,
            5,
            &["read_file", "bash"],
        ));
        trace.records.push_back(make_record(
            2,
            HookPoint::PostTurn,
            12_000,
            5,
            &["read_file", "bash"],
        ));
        trace.records.push_back(make_record(
            3,
            HookPoint::PostToolBatch,
            20_000,
            8,
            &["read_file", "bash", "edit_file"],
        ));
        trace.records.push_back(make_record(
            3,
            HookPoint::PostTurn,
            20_000,
            8,
            &["read_file", "bash", "edit_file"],
        ));
        trace.total_turns = 4;
        trace
    }

    #[test]
    fn rollback_to_turn_1_has_side_effects() {
        let trace = sample_trace();
        let assessment = trace.assess_rollback(1);
        assert_eq!(assessment.turns_to_reverse, 2);
        assert_eq!(assessment.tokens_to_reclaim, 15_000);
        assert_eq!(assessment.feasibility, RollbackFeasibility::HasSideEffects);
        assert!(assessment.tools_used_after_target.contains(&"bash".into()));
    }

    #[test]
    fn rollback_to_current_is_noop() {
        let trace = sample_trace();
        let assessment = trace.assess_rollback(3);
        assert_eq!(assessment.turns_to_reverse, 0);
        assert_eq!(assessment.tokens_to_reclaim, 0);
        assert_eq!(assessment.feasibility, RollbackFeasibility::Safe);
    }

    #[test]
    fn rollback_to_invalid_turn() {
        let trace = sample_trace();
        let assessment = trace.assess_rollback(99);
        assert_eq!(assessment.feasibility, RollbackFeasibility::InvalidTarget);
    }

    #[test]
    fn rollback_safe_when_only_reads() {
        let mut trace = SessionTrace::new("safe".into());
        trace.records.push_back(make_record(
            1,
            HookPoint::PostTurn,
            5_000,
            2,
            &["read_file"],
        ));
        trace.records.push_back(make_record(
            2,
            HookPoint::PostTurn,
            10_000,
            4,
            &["read_file", "search"],
        ));

        let assessment = trace.assess_rollback(1);
        assert_eq!(assessment.feasibility, RollbackFeasibility::Safe);
    }

    #[test]
    fn assessment_serializes() {
        let trace = sample_trace();
        let assessment = trace.assess_rollback(1);
        let json = serde_json::to_string(&assessment).unwrap();
        assert!(json.contains("HasSideEffects"));
    }
}
