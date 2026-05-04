use crate::RuntimeSnapshot;

/// Delta between two snapshots (from → to).
#[derive(Debug, Clone, serde::Serialize)]
pub struct SnapshotDiff {
    pub from_turn: u32,
    pub to_turn: u32,
    pub turns_delta: i64,
    pub tokens_delta: i64,
    pub elapsed_delta_millis: i64,
    pub tool_calls_delta: i32,
    pub new_tools: Vec<String>,
    pub context_utilization_delta: Option<f32>,
    pub consecutive_same_tool_changed: bool,
}

impl SnapshotDiff {
    pub fn between(from: &RuntimeSnapshot, to: &RuntimeSnapshot) -> Self {
        let from_tools: std::collections::HashSet<&str> =
            from.unique_tools_used.iter().map(|s| s.as_str()).collect();
        let new_tools: Vec<String> = to
            .unique_tools_used
            .iter()
            .filter(|t| !from_tools.contains(t.as_str()))
            .cloned()
            .collect();

        let context_utilization_delta = match (from.context_utilization, to.context_utilization) {
            (Some(f), Some(t)) => Some(t - f),
            _ => None,
        };

        Self {
            from_turn: from.turn_number,
            to_turn: to.turn_number,
            turns_delta: to.turns_used as i64 - from.turns_used as i64,
            tokens_delta: to.tokens_used_session as i64 - from.tokens_used_session as i64,
            elapsed_delta_millis: to.elapsed_millis as i64 - from.elapsed_millis as i64,
            tool_calls_delta: to.tool_calls_this_session as i32
                - from.tool_calls_this_session as i32,
            new_tools,
            context_utilization_delta,
            consecutive_same_tool_changed: from.consecutive_same_tool != to.consecutive_same_tool,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.turns_delta == 0
            && self.tokens_delta == 0
            && self.tool_calls_delta == 0
            && self.new_tools.is_empty()
            && !self.consecutive_same_tool_changed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap(turn: u32, tokens: u64, tool_calls: u32, tools: &[&str]) -> RuntimeSnapshot {
        RuntimeSnapshot {
            turn_number: turn,
            turns_used: turn,
            tokens_used_session: tokens,
            tool_calls_this_session: tool_calls,
            unique_tools_used: tools.iter().map(|s| s.to_string()).collect(),
            ..RuntimeSnapshot::empty()
        }
    }

    #[test]
    fn diff_detects_changes() {
        let from = snap(1, 10_000, 2, &["bash"]);
        let to = snap(3, 30_000, 5, &["bash", "read_file", "edit_file"]);
        let diff = SnapshotDiff::between(&from, &to);

        assert_eq!(diff.turns_delta, 2);
        assert_eq!(diff.tokens_delta, 20_000);
        assert_eq!(diff.tool_calls_delta, 3);
        assert_eq!(diff.new_tools, vec!["read_file", "edit_file"]);
        assert!(!diff.is_empty());
    }

    #[test]
    fn diff_identical_is_empty() {
        let a = snap(5, 50_000, 10, &["bash"]);
        let diff = SnapshotDiff::between(&a, &a);
        assert!(diff.is_empty());
    }

    #[test]
    fn diff_negative_delta() {
        let from = snap(5, 50_000, 10, &["bash", "read_file"]);
        let to = snap(3, 30_000, 5, &["bash"]);
        let diff = SnapshotDiff::between(&from, &to);

        assert_eq!(diff.turns_delta, -2);
        assert_eq!(diff.tokens_delta, -20_000);
        assert!(diff.new_tools.is_empty());
    }

    #[test]
    fn diff_context_utilization() {
        let mut from = snap(1, 0, 0, &[]);
        from.context_utilization = Some(0.3);
        let mut to = snap(2, 0, 0, &[]);
        to.context_utilization = Some(0.7);

        let diff = SnapshotDiff::between(&from, &to);
        let delta = diff.context_utilization_delta.unwrap();
        assert!((delta - 0.4).abs() < 0.001);
    }

    #[test]
    fn diff_consecutive_tool_change() {
        let mut from = snap(1, 0, 0, &[]);
        from.consecutive_same_tool = 0;
        let mut to = snap(2, 0, 0, &[]);
        to.consecutive_same_tool = 3;

        let diff = SnapshotDiff::between(&from, &to);
        assert!(diff.consecutive_same_tool_changed);
    }
}
