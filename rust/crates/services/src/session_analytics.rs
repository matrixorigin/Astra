//! Session analytics: compute stats from journal events.

use crate::session_journal::{JournalEvent, JournalEventType};

/// Aggregated stats for a single session.
#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct SessionStats {
    pub session_id: String,
    pub turn_count: u32,
    pub error_count: u32,
    pub stall_count: u32,
    pub checkpoint_count: u32,
    pub compact_count: u32,
    pub execution_boundary_opened_count: u32,
    pub execution_boundary_committed_count: u32,
    pub execution_boundary_aborted_count: u32,
    pub approval_required_count: u32,
    pub approval_decision_count: u32,
    pub approval_timeout_count: u32,
    pub total_tokens_in: u64,
    pub total_tokens_out: u64,
    pub total_cache_read: u64,
    pub total_cache_creation: u64,
    pub total_duration_ms: u64,
    pub total_tool_calls: u32,
    pub unique_tools: Vec<String>,
    pub failed_tool_calls: u32,
    pub model: Option<String>,
    /// Average tokens per turn (in + out).
    pub avg_tokens_per_turn: u64,
    /// Average duration per turn in ms.
    pub avg_duration_ms: u64,
    /// Tool error rate (0.0–1.0).
    pub tool_error_rate: f64,
}

/// Compute stats from a list of journal events for one session.
pub fn compute_session_stats(session_id: &str, events: &[JournalEvent]) -> SessionStats {
    let mut stats = SessionStats {
        session_id: session_id.to_string(),
        ..Default::default()
    };

    let mut tools_set = std::collections::HashSet::new();

    for event in events {
        match event.event_type {
            JournalEventType::SessionStart if stats.model.is_none() => {
                stats.model = event.model.clone();
            }
            JournalEventType::SessionStart => {}
            JournalEventType::Turn => {
                stats.turn_count += 1;
                stats.total_tokens_in += event.tokens_in.unwrap_or(0);
                stats.total_tokens_out += event.tokens_out.unwrap_or(0);
                stats.total_cache_read += event.cache_read_tokens.unwrap_or(0);
                stats.total_cache_creation += event.cache_creation_tokens.unwrap_or(0);
                stats.total_duration_ms += event.duration_ms.unwrap_or(0);
                // Synthetic placeholders (skill skipped/deferred, surgically
                // removed parallel calls) are not real tool executions — skip
                // them from total + failed counts so tool_error_rate reflects
                // actual tool reliability.
                if let Some(ref calls) = event.tool_calls {
                    let real_calls: Vec<_> = calls
                        .iter()
                        .filter(|tc| !tc.is_synthetic_placeholder())
                        .collect();
                    stats.total_tool_calls += real_calls.len() as u32;
                    for tc in &real_calls {
                        if !tc.ok {
                            stats.failed_tool_calls += 1;
                        }
                    }
                } else {
                    stats.total_tool_calls += event.tool_count.unwrap_or(0);
                }
                if let Some(ref tools) = event.tools_used {
                    for t in tools {
                        tools_set.insert(t.clone());
                    }
                }
            }
            JournalEventType::TurnError => stats.error_count += 1,
            JournalEventType::StallDetected => stats.stall_count += 1,
            JournalEventType::Checkpoint => stats.checkpoint_count += 1,
            JournalEventType::Compact => stats.compact_count += 1,
            JournalEventType::ExecutionBoundaryOpened => {
                stats.execution_boundary_opened_count += 1;
            }
            JournalEventType::ExecutionBoundaryCommitted => {
                stats.execution_boundary_committed_count += 1;
            }
            JournalEventType::ExecutionBoundaryAborted => {
                stats.execution_boundary_aborted_count += 1;
            }
            JournalEventType::ApprovalRequired => stats.approval_required_count += 1,
            JournalEventType::ApprovalDecision => stats.approval_decision_count += 1,
            JournalEventType::ApprovalTimeout => stats.approval_timeout_count += 1,
            _ => {}
        }
    }

    stats.unique_tools = tools_set.into_iter().collect();
    stats.unique_tools.sort();

    if stats.turn_count > 0 {
        let total_tokens = stats.total_tokens_in + stats.total_tokens_out;
        stats.avg_tokens_per_turn = total_tokens / stats.turn_count as u64;
        stats.avg_duration_ms = stats.total_duration_ms / stats.turn_count as u64;
    }
    if stats.total_tool_calls > 0 {
        stats.tool_error_rate = stats.failed_tool_calls as f64 / stats.total_tool_calls as f64;
    }

    stats
}

/// Summary across multiple sessions.
#[derive(Debug, Default, serde::Serialize)]
pub struct AggregateSummary {
    pub session_count: u32,
    pub total_turns: u32,
    pub total_errors: u32,
    pub total_stalls: u32,
    pub total_execution_boundaries_opened: u32,
    pub total_execution_boundaries_committed: u32,
    pub total_execution_boundaries_aborted: u32,
    pub total_approval_required: u32,
    pub total_approval_decisions: u32,
    pub total_approval_timeouts: u32,
    pub total_tokens_in: u64,
    pub total_tokens_out: u64,
    pub total_tool_calls: u32,
    pub total_failed_tools: u32,
    pub avg_turns_per_session: f64,
    pub overall_tool_error_rate: f64,
}

pub fn aggregate_stats(stats: &[SessionStats]) -> AggregateSummary {
    let mut agg = AggregateSummary {
        session_count: stats.len() as u32,
        ..Default::default()
    };
    for s in stats {
        agg.total_turns += s.turn_count;
        agg.total_errors += s.error_count;
        agg.total_stalls += s.stall_count;
        agg.total_execution_boundaries_opened += s.execution_boundary_opened_count;
        agg.total_execution_boundaries_committed += s.execution_boundary_committed_count;
        agg.total_execution_boundaries_aborted += s.execution_boundary_aborted_count;
        agg.total_approval_required += s.approval_required_count;
        agg.total_approval_decisions += s.approval_decision_count;
        agg.total_approval_timeouts += s.approval_timeout_count;
        agg.total_tokens_in += s.total_tokens_in;
        agg.total_tokens_out += s.total_tokens_out;
        agg.total_tool_calls += s.total_tool_calls;
        agg.total_failed_tools += s.failed_tool_calls;
    }
    if agg.session_count > 0 {
        agg.avg_turns_per_session = agg.total_turns as f64 / agg.session_count as f64;
    }
    if agg.total_tool_calls > 0 {
        agg.overall_tool_error_rate = agg.total_failed_tools as f64 / agg.total_tool_calls as f64;
    }
    agg
}

/// Per-tool performance profile.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct ToolProfile {
    pub name: String,
    pub call_count: u32,
    pub success_count: u32,
    pub fail_count: u32,
    pub total_ms: u64,
    pub min_ms: u64,
    pub max_ms: u64,
    pub avg_ms: u64,
    pub error_rate: f64,
    /// Most recent error message (if any).
    pub last_error: Option<String>,
}

/// Compute per-tool profiles from journal events.
pub fn compute_tool_profiles(events: &[JournalEvent]) -> Vec<ToolProfile> {
    let mut map: std::collections::HashMap<String, ToolProfile> = std::collections::HashMap::new();

    for event in events {
        if let Some(ref calls) = event.tool_calls {
            for tc in calls {
                let p = map.entry(tc.name.clone()).or_insert_with(|| ToolProfile {
                    name: tc.name.clone(),
                    min_ms: u64::MAX,
                    ..Default::default()
                });
                p.call_count += 1;
                if tc.ok {
                    p.success_count += 1;
                } else {
                    p.fail_count += 1;
                    if tc.error.is_some() {
                        p.last_error = tc.error.clone();
                    }
                }
                p.total_ms += tc.ms;
                p.min_ms = p.min_ms.min(tc.ms);
                p.max_ms = p.max_ms.max(tc.ms);
            }
        }
    }

    let mut profiles: Vec<_> = map
        .into_values()
        .map(|mut p| {
            if p.call_count > 0 {
                p.avg_ms = p.total_ms / p.call_count as u64;
                p.error_rate = p.fail_count as f64 / p.call_count as f64;
            }
            if p.min_ms == u64::MAX {
                p.min_ms = 0;
            }
            p
        })
        .collect();
    // Sort by total time descending (heaviest tools first).
    profiles.sort_by_key(|b| std::cmp::Reverse(b.total_ms));
    profiles
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session_journal::JournalEvent;

    fn make_turn(
        tokens_in: u64,
        tokens_out: u64,
        duration_ms: u64,
        tool_count: u32,
    ) -> JournalEvent {
        let mut e = JournalEvent::turn(
            Some("test-session"),
            1,
            Some("gpt-4"),
            "hello",
            "world",
            tool_count,
            tokens_in,
            tokens_out,
            duration_ms,
        );
        e.tools_used = Some(vec!["bash".into()]);
        e
    }

    #[test]
    fn empty_events_returns_zero_stats() {
        let stats = compute_session_stats("s1", &[]);
        assert_eq!(stats.turn_count, 0);
        assert_eq!(stats.total_tokens_in, 0);
        assert_eq!(stats.avg_tokens_per_turn, 0);
        assert_eq!(stats.tool_error_rate, 0.0);
    }

    #[test]
    fn single_turn_stats() {
        let events = vec![
            JournalEvent::session_start(Some("s1"), Some("gpt-4")),
            make_turn(1000, 500, 2000, 3),
        ];
        let stats = compute_session_stats("s1", &events);
        assert_eq!(stats.turn_count, 1);
        assert_eq!(stats.total_tokens_in, 1000);
        assert_eq!(stats.total_tokens_out, 500);
        assert_eq!(stats.total_duration_ms, 2000);
        assert_eq!(stats.total_tool_calls, 3);
        assert_eq!(stats.avg_tokens_per_turn, 1500);
        assert_eq!(stats.avg_duration_ms, 2000);
        assert_eq!(stats.model, Some("gpt-4".into()));
        assert!(stats.unique_tools.contains(&"bash".to_string()));
    }

    #[test]
    fn multi_turn_averages() {
        let events = vec![
            make_turn(1000, 500, 2000, 2),
            make_turn(2000, 1000, 4000, 4),
        ];
        let stats = compute_session_stats("s1", &events);
        assert_eq!(stats.turn_count, 2);
        assert_eq!(stats.total_tokens_in, 3000);
        assert_eq!(stats.avg_tokens_per_turn, 2250); // (3000+1500)/2
        assert_eq!(stats.avg_duration_ms, 3000);
    }

    #[test]
    fn error_and_stall_counts() {
        let events = vec![
            make_turn(100, 50, 500, 1),
            JournalEvent::turn_error(Some("s1"), 2, Some("gpt-4"), "hello", "timeout", 500),
            JournalEvent::stall_detected(Some("s1"), 3, "tool_repeat", 0, 0.9, &[]),
            JournalEvent::stall_detected(Some("s1"), 4, "no_progress", 0, 0.8, &[]),
        ];
        let stats = compute_session_stats("s1", &events);
        assert_eq!(stats.turn_count, 1);
        assert_eq!(stats.error_count, 1);
        assert_eq!(stats.stall_count, 2);
    }

    #[test]
    fn checkpoint_and_compact_counts() {
        let events = vec![
            JournalEvent::checkpoint(Some("s1"), 5, "Phase A done", 50000, 3),
            JournalEvent::checkpoint(Some("s1"), 10, "Phase B done", 100000, 5),
            JournalEvent::compact(Some("s1"), 5, 5, 3),
        ];
        let stats = compute_session_stats("s1", &events);
        assert_eq!(stats.checkpoint_count, 2);
        assert_eq!(stats.compact_count, 1);
    }

    #[test]
    fn execution_boundary_counts() {
        let events = vec![
            JournalEvent::execution_boundary_opened(
                Some("s1"),
                7,
                "tool_batch",
                Some("tx-1"),
                serde_json::json!({}),
            ),
            JournalEvent::execution_boundary_committed(
                Some("s1"),
                7,
                "tool_batch",
                Some("tx-1"),
                None,
            ),
            JournalEvent::execution_boundary_opened(
                Some("s1"),
                8,
                "turn_rollback",
                None,
                serde_json::json!({}),
            ),
            JournalEvent::execution_boundary_aborted(
                Some("s1"),
                8,
                "turn_rollback",
                None,
                "tool failed",
                Some("write_file"),
                Some("req-2"),
                None,
            ),
        ];
        let stats = compute_session_stats("s1", &events);
        assert_eq!(stats.execution_boundary_opened_count, 2);
        assert_eq!(stats.execution_boundary_committed_count, 1);
        assert_eq!(stats.execution_boundary_aborted_count, 1);
    }

    #[test]
    fn approval_counts() {
        let events = vec![
            JournalEvent::approval_required(
                Some("s1"),
                None,
                "req-1",
                "write_file",
                "standard",
                Some("write a file"),
            ),
            JournalEvent::approval_decision(
                Some("s1"),
                None,
                "req-1",
                Some("write_file"),
                Some("standard"),
                "allow",
                None,
            ),
            JournalEvent::approval_timeout(Some("s1"), None, "req-2", "bash", "explicit"),
        ];
        let stats = compute_session_stats("s1", &events);
        assert_eq!(stats.approval_required_count, 1);
        assert_eq!(stats.approval_decision_count, 1);
        assert_eq!(stats.approval_timeout_count, 1);
    }

    #[test]
    fn tool_error_rate_computation() {
        let mut turn = make_turn(100, 50, 500, 3);
        turn.tool_calls = Some(vec![
            crate::session_journal::ToolCallRecord {
                name: "bash".into(),
                ms: 100,
                ok: true,
                error: None,
                input_bytes: None,
                output_bytes: None,
                args_preview: None,
                result_preview: None,
                file_path: None,
                surgically_removed: None,
                original_tool_name: None,
            },
            crate::session_journal::ToolCallRecord {
                name: "grep".into(),
                ms: 50,
                ok: true,
                error: None,
                input_bytes: None,
                output_bytes: None,
                args_preview: None,
                result_preview: None,
                file_path: None,
                surgically_removed: None,
                original_tool_name: None,
            },
            crate::session_journal::ToolCallRecord {
                name: "write_file".into(),
                ms: 200,
                ok: false,
                error: Some("permission denied".into()),
                input_bytes: None,
                output_bytes: None,
                args_preview: None,
                result_preview: None,
                file_path: None,
                surgically_removed: None,
                original_tool_name: None,
            },
        ]);
        let stats = compute_session_stats("s1", &[turn]);
        assert_eq!(stats.failed_tool_calls, 1);
        assert_eq!(stats.total_tool_calls, 3);
        assert!((stats.tool_error_rate - 1.0 / 3.0).abs() < 0.01);
    }

    #[test]
    fn unique_tools_deduped_and_sorted() {
        let mut t1 = make_turn(100, 50, 500, 2);
        t1.tools_used = Some(vec!["grep".into(), "bash".into()]);
        let mut t2 = make_turn(100, 50, 500, 1);
        t2.tools_used = Some(vec!["bash".into(), "read_file".into()]);
        let stats = compute_session_stats("s1", &[t1, t2]);
        assert_eq!(stats.unique_tools, vec!["bash", "grep", "read_file"]);
    }

    #[test]
    fn aggregate_across_sessions() {
        let s1 = SessionStats {
            turn_count: 10,
            error_count: 1,
            stall_count: 2,
            execution_boundary_opened_count: 4,
            execution_boundary_committed_count: 3,
            execution_boundary_aborted_count: 1,
            approval_required_count: 3,
            approval_decision_count: 2,
            approval_timeout_count: 1,
            total_tokens_in: 5000,
            total_tokens_out: 3000,
            total_tool_calls: 20,
            failed_tool_calls: 2,
            ..Default::default()
        };
        let s2 = SessionStats {
            turn_count: 5,
            error_count: 0,
            stall_count: 0,
            execution_boundary_opened_count: 2,
            execution_boundary_committed_count: 1,
            execution_boundary_aborted_count: 1,
            approval_required_count: 1,
            approval_decision_count: 1,
            approval_timeout_count: 0,
            total_tokens_in: 2000,
            total_tokens_out: 1000,
            total_tool_calls: 10,
            failed_tool_calls: 1,
            ..Default::default()
        };
        let agg = aggregate_stats(&[s1, s2]);
        assert_eq!(agg.session_count, 2);
        assert_eq!(agg.total_turns, 15);
        assert_eq!(agg.total_errors, 1);
        assert_eq!(agg.total_stalls, 2);
        assert_eq!(agg.total_execution_boundaries_opened, 6);
        assert_eq!(agg.total_execution_boundaries_committed, 4);
        assert_eq!(agg.total_execution_boundaries_aborted, 2);
        assert_eq!(agg.total_approval_required, 4);
        assert_eq!(agg.total_approval_decisions, 3);
        assert_eq!(agg.total_approval_timeouts, 1);
        assert_eq!(agg.total_tokens_in, 7000);
        assert_eq!(agg.total_tool_calls, 30);
        assert_eq!(agg.total_failed_tools, 3);
        assert!((agg.avg_turns_per_session - 7.5).abs() < 0.01);
        assert!((agg.overall_tool_error_rate - 0.1).abs() < 0.01);
    }

    // ── Tool profiling tests ──────────────────────────────────────────────

    fn make_tool_call(name: &str, ms: u64, ok: bool) -> crate::session_journal::ToolCallRecord {
        crate::session_journal::ToolCallRecord {
            name: name.into(),
            ms,
            ok,
            error: if ok { None } else { Some("fail".into()) },
            input_bytes: None,
            output_bytes: None,
            args_preview: None,
            result_preview: None,
            file_path: None,
            surgically_removed: None,
            original_tool_name: None,
        }
    }

    #[test]
    fn tool_profiles_empty_events() {
        let profiles = compute_tool_profiles(&[]);
        assert!(profiles.is_empty());
    }

    #[test]
    fn tool_profiles_basic() {
        let mut t = make_turn(100, 50, 500, 2);
        t.tool_calls = Some(vec![
            make_tool_call("bash", 200, true),
            make_tool_call("bash", 300, true),
            make_tool_call("grep", 50, true),
            make_tool_call("grep", 100, false),
        ]);
        let profiles = compute_tool_profiles(&[t]);
        assert_eq!(profiles.len(), 2);
        // bash has more total_ms, should be first
        let bash = profiles.iter().find(|p| p.name == "bash").unwrap();
        assert_eq!(bash.call_count, 2);
        assert_eq!(bash.success_count, 2);
        assert_eq!(bash.fail_count, 0);
        assert_eq!(bash.total_ms, 500);
        assert_eq!(bash.min_ms, 200);
        assert_eq!(bash.max_ms, 300);
        assert_eq!(bash.avg_ms, 250);
        assert_eq!(bash.error_rate, 0.0);

        let grep = profiles.iter().find(|p| p.name == "grep").unwrap();
        assert_eq!(grep.call_count, 2);
        assert_eq!(grep.fail_count, 1);
        assert!((grep.error_rate - 0.5).abs() < 0.01);
        assert!(grep.last_error.is_some());
    }

    #[test]
    fn tool_profiles_sorted_by_total_time() {
        let mut t = make_turn(100, 50, 500, 3);
        t.tool_calls = Some(vec![
            make_tool_call("fast_tool", 10, true),
            make_tool_call("slow_tool", 5000, true),
            make_tool_call("mid_tool", 500, true),
        ]);
        let profiles = compute_tool_profiles(&[t]);
        assert_eq!(profiles[0].name, "slow_tool");
        assert_eq!(profiles[1].name, "mid_tool");
        assert_eq!(profiles[2].name, "fast_tool");
    }

    #[test]
    fn tool_profiles_across_turns() {
        let mut t1 = make_turn(100, 50, 500, 1);
        t1.tool_calls = Some(vec![make_tool_call("bash", 100, true)]);
        let mut t2 = make_turn(100, 50, 500, 1);
        t2.tool_calls = Some(vec![make_tool_call("bash", 400, false)]);
        let profiles = compute_tool_profiles(&[t1, t2]);
        assert_eq!(profiles.len(), 1);
        let bash = &profiles[0];
        assert_eq!(bash.call_count, 2);
        assert_eq!(bash.total_ms, 500);
        assert_eq!(bash.min_ms, 100);
        assert_eq!(bash.max_ms, 400);
        assert_eq!(bash.fail_count, 1);
    }
}
