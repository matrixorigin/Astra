//! Incremental lesson extraction at natural breakpoints.
//!
//! Instead of waiting until session end to extract lessons, this module
//! checks for lesson-worthy signals at key moments during the session:
//! user corrections, stall recovery, plan completion. Each checkpoint
//! produces a delta (new lessons since the last checkpoint) and
//! deduplicates within the session to prevent double-recording.

use std::collections::HashSet;

use crate::lesson_extractor::{self, SessionSummary};
use astra_services::{LessonKind, NewLesson};

/// Tracks lesson extraction state across a session so breakpoints
/// can produce deltas without re-extracting already-recorded lessons.
#[derive(Debug, Default)]
pub struct LessonCheckpointer {
    last_checkpoint_turn: u32,
    recorded_keys: HashSet<(LessonKind, String)>,
}

impl LessonCheckpointer {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Check if lessons should be extracted at this turn. Returns the
    /// delta: only lessons not yet recorded in this session.
    ///
    /// Callers provide a fresh `SessionSummary` snapshot. The checkpointer
    /// compares against previously recorded `(kind, trigger_signal)` pairs
    /// and returns only new lessons. This is idempotent: calling with the
    /// same summary twice returns an empty vec the second time.
    pub fn maybe_checkpoint(
        &mut self,
        summary: &SessionSummary,
        turn: u32,
        user_id: &str,
        persona: &str,
        workload_tag: Option<&str>,
    ) -> Vec<NewLesson> {
        if turn <= self.last_checkpoint_turn {
            return Vec::new();
        }

        let all = lesson_extractor::extract_lessons(summary, user_id, persona, workload_tag);
        let delta: Vec<NewLesson> = all
            .into_iter()
            .filter(|l| {
                let key = (l.kind, l.trigger_signal.clone());
                self.recorded_keys.insert(key)
            })
            .collect();

        if !delta.is_empty() {
            self.last_checkpoint_turn = turn;
        }

        delta
    }

    /// Number of unique lessons recorded so far in this session.
    #[must_use]
    pub fn recorded_count(&self) -> usize {
        self.recorded_keys.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn summary_with_tool_failures(tools: &[(&str, u32)]) -> SessionSummary {
        let mut s = SessionSummary::default();
        for &(name, count) in tools {
            s.tool_failures.insert(name.to_string(), count);
        }
        s
    }

    #[test]
    fn first_checkpoint_extracts_all_lessons() {
        let mut cp = LessonCheckpointer::new();
        let summary = summary_with_tool_failures(&[("grep", 5), ("rg", 3)]);
        let delta = cp.maybe_checkpoint(&summary, 10, "u1", "generic", None);
        assert_eq!(delta.len(), 2, "both tools should produce lessons");
        assert_eq!(cp.recorded_count(), 2);
    }

    #[test]
    fn second_checkpoint_same_summary_returns_empty() {
        let mut cp = LessonCheckpointer::new();
        let summary = summary_with_tool_failures(&[("grep", 5)]);
        let first = cp.maybe_checkpoint(&summary, 10, "u1", "generic", None);
        assert_eq!(first.len(), 1);

        let second = cp.maybe_checkpoint(&summary, 11, "u1", "generic", None);
        assert!(second.is_empty(), "same lesson should be deduped");
    }

    #[test]
    fn new_tool_failure_produces_delta() {
        let mut cp = LessonCheckpointer::new();
        let summary1 = summary_with_tool_failures(&[("grep", 5)]);
        cp.maybe_checkpoint(&summary1, 10, "u1", "generic", None);

        let mut summary2 = summary_with_tool_failures(&[("grep", 5), ("rg", 3)]);
        summary2.stall_events = 3; // also triggers a PromptShape lesson
        let delta = cp.maybe_checkpoint(&summary2, 15, "u1", "generic", None);
        assert_eq!(delta.len(), 2, "rg + stall should be new");
        assert!(delta.iter().any(|l| l.trigger_signal.contains("rg")));
        assert!(delta.iter().any(|l| l.kind == LessonKind::PromptShape));
    }

    #[test]
    fn same_turn_does_not_re_checkpoint() {
        let mut cp = LessonCheckpointer::new();
        let summary = summary_with_tool_failures(&[("grep", 5)]);
        cp.maybe_checkpoint(&summary, 10, "u1", "generic", None);

        let delta = cp.maybe_checkpoint(&summary, 10, "u1", "generic", None);
        assert!(delta.is_empty(), "same turn should not re-extract");
    }

    #[test]
    fn subthreshold_signals_produce_no_lessons() {
        let mut cp = LessonCheckpointer::new();
        let summary = summary_with_tool_failures(&[("grep", 2)]); // below threshold (3)
        let delta = cp.maybe_checkpoint(&summary, 10, "u1", "generic", None);
        assert!(delta.is_empty());
        assert_eq!(cp.recorded_count(), 0);
    }

    #[test]
    fn recorded_count_tracks_unique_lessons() {
        let mut cp = LessonCheckpointer::new();
        let mut summary = SessionSummary::default();
        summary.tool_failures.insert("grep".into(), 5);
        summary.stall_events = 3;
        summary.user_corrections = vec!["fix a".into(), "fix b".into()];
        summary.unmet_postconditions = 4;

        cp.maybe_checkpoint(&summary, 10, "u1", "generic", None);
        assert_eq!(cp.recorded_count(), 4); // tool + stall + corrections + postconditions
    }
}
