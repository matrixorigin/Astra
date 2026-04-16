use super::*;

// ── slash_stats::handle_stats_command ─────────────────────────────────────────────────

#[test]
fn stats_no_active_session_does_not_panic() {
    // state with no session_id → should not panic
    let state = super::ReplState::default();
    tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(slash_stats::handle_stats_command("", &state)); // current session mode, no session
}

#[test]
fn stats_history_no_sessions_does_not_panic() {
    let state = super::ReplState::default();
    tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(slash_stats::handle_stats_command("history", &state));
}

#[test]
fn stats_current_session_reads_journal() {
    let _creds = isolate_credentials();
    use astra_services::session_analytics;

    // Create a real journal with known events
    let sid = format!("test-stats-{}", uuid::Uuid::new_v4());
    let writer = session_journal::JournalWriter::new(&sid).unwrap();
    writer
        .append(&session_journal::JournalEvent::session_start(
            Some(&sid),
            Some("gpt-4o"),
        ))
        .unwrap();
    writer
        .append(&session_journal::JournalEvent::turn(
            Some(&sid),
            1,
            Some("gpt-4o"),
            "hello",
            "hi",
            2,
            1000,
            500,
            1500,
        ))
        .unwrap();
    writer
        .append(&session_journal::JournalEvent::turn(
            Some(&sid),
            2,
            Some("gpt-4o"),
            "what is rust?",
            "a systems language",
            1,
            800,
            400,
            1200,
        ))
        .unwrap();
    drop(writer);

    // Verify the analytics layer computes correctly from these events
    let events = session_journal::read_journal(&sid).unwrap();
    let stats = session_analytics::compute_session_stats(&sid, &events);

    assert_eq!(stats.turn_count, 2);
    assert_eq!(stats.total_tokens_in, 1800);
    assert_eq!(stats.total_tokens_out, 900);
    assert_eq!(stats.total_tool_calls, 3);
    assert_eq!(stats.model, Some("gpt-4o".into()));
    assert_eq!(stats.avg_tokens_per_turn, 1350); // (1800+900)/2

    // Now verify slash_stats::handle_stats_command doesn't panic with this session
    let state = super::ReplState {
        session_id: Some(sid),
        ..Default::default()
    };
    tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(slash_stats::handle_stats_command("", &state));
}

#[test]
fn stats_history_aggregates_multiple_sessions() {
    let _creds = isolate_credentials();
    use astra_services::session_analytics;

    // Create two sessions
    let sid1 = format!("test-stats-hist-a-{}", uuid::Uuid::new_v4());
    let sid2 = format!("test-stats-hist-b-{}", uuid::Uuid::new_v4());

    for sid in [&sid1, &sid2] {
        let writer = session_journal::JournalWriter::new(sid).unwrap();
        writer
            .append(&session_journal::JournalEvent::turn(
                Some(sid),
                1,
                None,
                "q",
                "a",
                1,
                500,
                250,
                800,
            ))
            .unwrap();
        drop(writer);
    }

    let e1 = session_journal::read_journal(&sid1).unwrap();
    let e2 = session_journal::read_journal(&sid2).unwrap();
    let s1 = session_analytics::compute_session_stats(&sid1, &e1);
    let s2 = session_analytics::compute_session_stats(&sid2, &e2);
    let agg = session_analytics::aggregate_stats(&[s1, s2]);

    assert_eq!(agg.session_count, 2);
    assert_eq!(agg.total_turns, 2);
    assert_eq!(agg.total_tokens_in, 1000);
    assert_eq!(agg.total_tokens_out, 500);
}

// ── slash_tools::handle_tools_command ─────────────────────────────────────────────────

#[test]
fn tools_no_active_session_does_not_panic() {
    let state = super::ReplState::default();
    slash_tools::handle_tools_command(&state);
}

#[test]
fn tools_session_with_no_tool_calls_does_not_panic() {
    let _creds = isolate_credentials();
    let sid = format!("test-tools-empty-{}", uuid::Uuid::new_v4());
    let writer = session_journal::JournalWriter::new(&sid).unwrap();
    writer
        .append(&session_journal::JournalEvent::turn(
            Some(&sid),
            1,
            None,
            "hello",
            "hi",
            0,
            100,
            50,
            500,
        ))
        .unwrap();
    drop(writer);

    let state = super::ReplState {
        session_id: Some(sid),
        ..Default::default()
    };
    slash_tools::handle_tools_command(&state);
}

#[test]
fn tools_reads_tool_calls_from_journal() {
    let _creds = isolate_credentials();
    use astra_services::session_analytics;

    let sid = format!("test-tools-calls-{}", uuid::Uuid::new_v4());
    let writer = session_journal::JournalWriter::new(&sid).unwrap();

    let mut event = session_journal::JournalEvent::turn(
        Some(&sid),
        1,
        None,
        "run tests",
        "done",
        3,
        500,
        200,
        3000,
    );
    event.tool_calls = Some(vec![
        session_journal::ToolCallRecord {
            name: "bash".into(),
            ms: 1000,
            ok: true,
            error: None,
            input_bytes: Some(50),
            output_bytes: Some(200),
            args_preview: Some("npm test".into()),
            result_preview: None,
            file_path: None,
        },
        session_journal::ToolCallRecord {
            name: "bash".into(),
            ms: 2000,
            ok: false,
            error: Some("exit code 1".into()),
            input_bytes: Some(30),
            output_bytes: Some(100),
            args_preview: Some("cargo build".into()),
            result_preview: None,
            file_path: None,
        },
        session_journal::ToolCallRecord {
            name: "grep".into(),
            ms: 50,
            ok: true,
            error: None,
            input_bytes: Some(20),
            output_bytes: Some(500),
            args_preview: Some("/error/ in src/".into()),
            result_preview: None,
            file_path: None,
        },
    ]);
    writer.append(&event).unwrap();
    drop(writer);

    // Verify analytics layer computes correctly
    let events = session_journal::read_journal(&sid).unwrap();
    let profiles = session_analytics::compute_tool_profiles(&events);

    assert_eq!(profiles.len(), 2);
    // sorted by total_ms descending: bash (3000ms) > grep (50ms)
    assert_eq!(profiles[0].name, "bash");
    assert_eq!(profiles[0].call_count, 2);
    assert_eq!(profiles[0].fail_count, 1);
    assert_eq!(profiles[0].total_ms, 3000);
    assert_eq!(profiles[0].min_ms, 1000);
    assert_eq!(profiles[0].max_ms, 2000);
    assert!((profiles[0].error_rate - 0.5).abs() < 0.01);
    assert_eq!(profiles[0].last_error, Some("exit code 1".into()));

    assert_eq!(profiles[1].name, "grep");
    assert_eq!(profiles[1].call_count, 1);
    assert_eq!(profiles[1].fail_count, 0);
    assert_eq!(profiles[1].error_rate, 0.0);

    // Verify slash_tools::handle_tools_command doesn't panic with this data
    let state = super::ReplState {
        session_id: Some(sid),
        ..Default::default()
    };
    slash_tools::handle_tools_command(&state);
}
