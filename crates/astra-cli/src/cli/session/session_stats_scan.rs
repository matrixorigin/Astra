use crate::cli::session::session_state::SessionState;
use astra_services::{session_analytics, session_journal};

pub(crate) fn current_rate_cost_rows(state: &SessionState) -> Vec<(&'static str, String)> {
    let pricing = &state.cached_pricing;
    let amount = |usage: [u64; 4]| {
        format_optional_cost(pricing.estimated_cost_usd(usage[0], usage[1], usage[2], usage[3]))
    };
    let mut rows = vec![
        ("basis", "observed counters".into()),
        ("billing", "not a session bill".into()),
        ("coverage", "unknown".into()),
        ("attribution", "unknown".into()),
        (
            "model",
            state.model.clone().unwrap_or_else(|| "<unset>".into()),
        ),
    ];
    for (label, count, index) in [
        ("fresh input", state.total_prompt_tokens, 0),
        ("output", state.total_completion_tokens, 1),
        ("cache read", state.total_cache_read_tokens, 2),
        ("cache write", state.total_cache_creation_tokens, 3),
    ] {
        let mut usage = [0; 4];
        usage[index] = count;
        rows.push((label, format!("{count} ({})", amount(usage))));
    }
    rows.push((
        "scenario sum",
        amount([
            state.total_prompt_tokens,
            state.total_completion_tokens,
            state.total_cache_read_tokens,
            state.total_cache_creation_tokens,
        ]),
    ));
    rows
}

/// Format a dollar cost for display.
pub(crate) fn format_optional_cost(cost: Option<f64>) -> String {
    cost.filter(|cost| cost.is_finite() && *cost >= 0.0)
        .map(format_cost)
        .unwrap_or_else(|| "unavailable".into())
}

/// Format a known dollar cost for display.
pub(crate) fn format_cost(cost: f64) -> String {
    if cost < 0.01 {
        format!("${:.4}", cost)
    } else if cost < 1.0 {
        format!("${:.3}", cost)
    } else {
        format!("${:.2}", cost)
    }
}

#[derive(Debug, Clone)]
pub(crate) struct UnreadableSessionJournal {
    pub(crate) session_id: String,
    pub(crate) error: String,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct RecentSessionStatsScan {
    pub(crate) stats: Vec<astra_services::session_analytics::SessionStats>,
    pub(crate) unreadable: Vec<UnreadableSessionJournal>,
}

pub(crate) fn read_session_journal_for_stats(
    session_id: &str,
) -> Result<Vec<session_journal::JournalEvent>, String> {
    session_journal::read_journal(session_id)
        .map_err(|error| format!("failed to read session journal for {session_id}: {error}"))
}

pub(crate) fn list_recent_session_ids_for_stats(limit: usize) -> Result<Vec<String>, String> {
    session_journal::list_sessions_by_time(limit.max(1))
        .map_err(|error| format!("failed to scan local sessions: {error}"))
}

pub(crate) fn collect_recent_session_stats(limit: usize) -> Result<RecentSessionStatsScan, String> {
    let session_ids = list_recent_session_ids_for_stats(limit)?;
    let mut scan = RecentSessionStatsScan::default();

    for session_id in &session_ids {
        match read_session_journal_for_stats(session_id) {
            Ok(events) => scan.stats.push(session_analytics::compute_session_stats(
                session_id, &events,
            )),
            Err(error) => scan.unreadable.push(UnreadableSessionJournal {
                session_id: session_id.clone(),
                error,
            }),
        }
    }

    Ok(scan)
}

#[cfg(test)]
mod tests {
    use super::{
        collect_recent_session_stats, list_recent_session_ids_for_stats,
        read_session_journal_for_stats,
    };
    use astra_services::session_journal::{self, JournalDirGuard};

    fn write_stats_session(session_id: &str) {
        let writer = session_journal::JournalWriter::new(session_id).unwrap();
        writer
            .append(&session_journal::JournalEvent::session_start(
                Some(session_id),
                Some("gpt-5"),
            ))
            .unwrap();
        writer
            .append(&session_journal::JournalEvent::turn(
                Some(session_id),
                1,
                Some("gpt-5"),
                "continue",
                "restored",
                0,
                15,
                7,
                8,
            ))
            .unwrap();
    }

    #[test]
    #[serial_test::serial]
    fn recent_stats_preserve_session_counters_and_history_totals() {
        let (_tmp, _guard) = crate::tests::isolated_sessions_dir();
        write_stats_session("stats-a");
        write_stats_session("stats-b");
        session_journal::JournalWriter::new("stats-a")
            .unwrap()
            .append(&session_journal::JournalEvent::turn(
                Some("stats-a"),
                2,
                Some("gpt-5"),
                "question",
                "answer",
                3,
                1000,
                500,
                1500,
            ))
            .unwrap();

        let scan = collect_recent_session_stats(10).unwrap();
        assert!(scan.unreadable.is_empty());
        let current = scan
            .stats
            .iter()
            .find(|stats| stats.session_id == "stats-a")
            .unwrap();
        assert_eq!(current.turn_count, 2);
        assert_eq!(current.total_tokens_in, 1015);
        assert_eq!(current.total_tokens_out, 507);
        assert_eq!(current.total_tool_calls, 3);
        assert_eq!(current.model.as_deref(), Some("gpt-5"));
        assert_eq!(current.avg_tokens_per_turn, 761);

        let totals = astra_services::session_analytics::aggregate_stats(&scan.stats);
        assert_eq!(totals.session_count, 2);
        assert_eq!(totals.total_turns, 3);
        assert_eq!(totals.total_tokens_in, 1030);
        assert_eq!(totals.total_tokens_out, 514);
    }

    #[test]
    #[serial_test::serial]
    fn stats_journal_retains_tool_timing_and_failure_evidence() {
        let (_tmp, _guard) = crate::tests::isolated_sessions_dir();
        let mut event = session_journal::JournalEvent::turn(
            Some("stats-tools"),
            1,
            None,
            "question",
            "answer",
            3,
            500,
            200,
            3000,
        );
        event.tool_calls = Some(
            [
                ("bash", 1000, true),
                ("bash", 2000, false),
                ("grep", 50, true),
            ]
            .into_iter()
            .map(|(name, ms, ok)| session_journal::ToolCallRecord {
                name: name.into(),
                ms,
                ok,
                error: (!ok).then(|| "exit code 1".into()),
                ..Default::default()
            })
            .collect(),
        );
        session_journal::JournalWriter::new("stats-tools")
            .unwrap()
            .append(&event)
            .unwrap();
        let events = read_session_journal_for_stats("stats-tools").unwrap();
        let profiles = astra_services::session_analytics::compute_tool_profiles(&events);
        assert_eq!(profiles.len(), 2);
        let bash = &profiles[0];
        assert_eq!(bash.name, "bash");
        assert_eq!(bash.call_count, 2);
        assert_eq!(bash.fail_count, 1);
        assert_eq!(
            (bash.total_ms, bash.min_ms, bash.max_ms),
            (3000, 1000, 2000)
        );
        assert!((bash.error_rate - 0.5).abs() < 0.01);
        assert_eq!(bash.last_error.as_deref(), Some("exit code 1"));
        let grep = &profiles[1];
        assert_eq!(grep.name, "grep");
        assert_eq!(grep.call_count, 1);
        assert_eq!(grep.fail_count, 0);
        assert_eq!(grep.error_rate, 0.0);
    }

    #[test]
    #[serial_test::serial]
    fn list_recent_session_ids_for_stats_surfaces_scan_error() {
        let tmp = tempfile::tempdir().unwrap();
        let _guard = JournalDirGuard::new(tmp.path());
        let owner_sessions_root = session_journal::local_owner_sessions_dir();
        std::fs::create_dir_all(owner_sessions_root.parent().unwrap()).unwrap();
        std::fs::write(&owner_sessions_root, "not-a-directory").unwrap();

        let error =
            list_recent_session_ids_for_stats(10).expect_err("session scan failure should surface");

        assert!(error.contains("failed to scan local sessions"), "{error}");
    }

    #[test]
    #[serial_test::serial]
    fn collect_recent_session_stats_marks_unreadable_journals() {
        let (_tmp, _guard) = crate::tests::isolated_sessions_dir();
        let good_session = format!("stats-good-{}", uuid::Uuid::new_v4());
        let bad_session = format!("stats-bad-{}", uuid::Uuid::new_v4());
        write_stats_session(&good_session);
        std::fs::create_dir_all(session_journal::journal_file_path(&bad_session)).unwrap();

        let scan = collect_recent_session_stats(10).expect("scan should succeed");

        assert_eq!(scan.stats.len(), 1);
        assert_eq!(scan.stats[0].session_id, good_session);
        assert_eq!(scan.unreadable.len(), 1);
        assert_eq!(scan.unreadable[0].session_id, bad_session);
        assert!(
            scan.unreadable[0]
                .error
                .contains("failed to read session journal"),
            "{}",
            scan.unreadable[0].error
        );
    }

    #[test]
    #[serial_test::serial]
    fn read_session_journal_for_stats_surfaces_directory_error() {
        let (_tmp, _guard) = crate::tests::isolated_sessions_dir();
        let session_id = format!("stats-dir-{}", uuid::Uuid::new_v4());
        std::fs::create_dir_all(session_journal::journal_file_path(&session_id)).unwrap();

        let error = read_session_journal_for_stats(&session_id)
            .expect_err("directory journal path should fail to read");

        assert!(error.contains("failed to read session journal"), "{error}");
    }
}
