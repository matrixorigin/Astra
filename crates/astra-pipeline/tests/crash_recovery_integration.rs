//! Integration tests for crash recovery pipeline.
//!
//! Sets up a real session journal (via JournalDirGuard), writes checkpoints
//! and tool-call events, then exercises `recover_from_crash` end-to-end.
//!
//! Scenarios:
//! - auto_recovery_pure_read_tools: all in-flight tools are pure-read → auto-recover
//! - requires_user_input_side_effect_in_flight: side-effect tool in-flight → requires user input
//! - no_checkpoint_returns_none: no checkpoint → Ok(None)
//! - completed_tools_auto_recover: all tools completed → auto-recover

use astra_pipeline::crash_recovery::{RecoveryOutcome, recover_from_crash};
use astra_pipeline::step_checkpoint::write_step_checkpoint;
use astra_pipeline::step_protocol::{
    ExecutionCursor, HeavyCheckpoint, LightCheckpoint, StepCheckpoint, StepEvent, StepEventType,
};
use astra_services::session_journal::JournalDirGuard;

const TEST_USER_ID: &str = "test-user";

/// Helper: write a minimal heavy checkpoint for a session.
fn write_test_heavy_checkpoint(session_id: &str, step_id: &str, created_at: u64) {
    let light = LightCheckpoint {
        protocol_version: astra_pipeline::step_protocol::PROTOCOL_VERSION,
        cursor: ExecutionCursor::default(),
        step_id: step_id.to_string(),
        task_id: "test-task".to_string(),
        agent_id: "test-agent".to_string(),
        progress: 0.0,
        total_tokens: 0,
        created_at,
    };

    let checkpoint = StepCheckpoint::Heavy(Box::new(HeavyCheckpoint {
        light,
        conversation_cursor: None,
        messages: vec![],
        budget_remaining_tokens: 0,
        budget_remaining_rounds: 0,
        blocked_tools: vec![],
        recent_tools: vec![],
        activated_deferred_tool_names: vec![],
        memory_context: None,
        delegation_id: None,
        delegation_pattern: None,
        delegation_sub_run_summaries: vec![],
        interruption: None,
        approval_overrides: None,
        consecutive_context_window_errors: 0,
        pipeline_state: None,
        compaction_state: None,
        config_version_id: None,
        workspace_observation_quarantine: None,
    }));

    write_step_checkpoint(TEST_USER_ID, session_id, 1, &checkpoint).unwrap();
}

/// Helper: write tool-call events to the session journal.
fn write_tool_events(session_id: &str, events: &[StepEvent]) {
    use astra_pipeline::step_checkpoint::FileBackedEventStore;
    use astra_pipeline::step_protocol::StepEventStore;
    let mut store = FileBackedEventStore::empty(TEST_USER_ID, session_id);
    for event in events {
        let _ = store.append(event.clone());
    }
}

/// Helper: create a minimal StepEvent.
fn make_step_event(
    event_id: &str,
    step_id: &str,
    event_type: StepEventType,
    created_at: u64,
    payload: Option<serde_json::Value>,
) -> StepEvent {
    StepEvent {
        event_id: event_id.to_string(),
        run_id: "test-run".into(),
        canonical_event_id: None,
        step_id: step_id.to_string(),
        event_type,
        agent_id: None,
        caused_by: vec![],
        payload,
        created_at,
    }
}

// ── Happy path: auto-recover with pure-read tools ──────────────────────────

#[test]
fn auto_recovery_pure_read_tools_all_completed() {
    let temp = tempfile::tempdir().unwrap();
    let _guard = JournalDirGuard::new(temp.path());
    let sid = "cr-int-pure-read";

    // Write checkpoint at t=1000
    write_test_heavy_checkpoint(sid, "session-turn-3-step-1", 1000);

    // Write tool-call events after checkpoint (all completed, all pure-read)
    let events = vec![
        make_step_event(
            "ev-1",
            "session-turn-4-step-1",
            StepEventType::ToolCallStarted,
            2000,
            Some(serde_json::json!({"tool_name": "read_file"})),
        ),
        make_step_event(
            "ev-2",
            "session-turn-4-step-1",
            StepEventType::ToolCallCompleted,
            2500,
            Some(serde_json::json!({"tool_name": "read_file", "result": "ok"})),
        ),
        make_step_event(
            "ev-3",
            "session-turn-4-step-2",
            StepEventType::ToolCallStarted,
            3000,
            Some(serde_json::json!({"tool_name": "grep"})),
        ),
        make_step_event(
            "ev-4",
            "session-turn-4-step-2",
            StepEventType::ToolCallCompleted,
            3500,
            Some(serde_json::json!({"tool_name": "grep", "result": "found"})),
        ),
    ];
    write_tool_events(sid, &events);

    let outcome = recover_from_crash(TEST_USER_ID, sid).unwrap();
    assert!(
        matches!(outcome, Some(RecoveryOutcome::AutoRecovered { .. })),
        "expected AutoRecovered for all-completed pure-read tools, got {:?}",
        outcome.map(|o| format!("{:?}", o))
    );
}

// ── Happy path: auto-recover with idempotent writes ────────────────────────

#[test]
fn auto_recovery_idempotent_write_tools_completed() {
    let temp = tempfile::tempdir().unwrap();
    let _guard = JournalDirGuard::new(temp.path());
    let sid = "cr-int-idempotent";

    write_test_heavy_checkpoint(sid, "session-turn-2-step-1", 1000);

    let events = vec![
        make_step_event(
            "ev-1",
            "session-turn-3-step-1",
            StepEventType::ToolCallStarted,
            2000,
            Some(serde_json::json!({"tool_name": "write_file"})),
        ),
        make_step_event(
            "ev-2",
            "session-turn-3-step-1",
            StepEventType::ToolCallCompleted,
            2500,
            Some(serde_json::json!({"tool_name": "write_file", "result": "written"})),
        ),
    ];
    write_tool_events(sid, &events);

    let outcome = recover_from_crash(TEST_USER_ID, sid).unwrap();
    assert!(
        matches!(outcome, Some(RecoveryOutcome::AutoRecovered { .. })),
        "expected AutoRecovered for completed idempotent writes, got {:?}",
        outcome.map(|o| format!("{:?}", o))
    );
}

// ── Requires user input: side-effect tool in-flight ────────────────────────

#[test]
fn requires_user_input_side_effect_tool_in_flight() {
    let temp = tempfile::tempdir().unwrap();
    let _guard = JournalDirGuard::new(temp.path());
    let sid = "cr-int-side-effect";

    write_test_heavy_checkpoint(sid, "session-turn-5-step-1", 1000);

    // bash started but never completed → in-flight at crash
    let events = vec![make_step_event(
        "ev-1",
        "session-turn-6-step-1",
        StepEventType::ToolCallStarted,
        2000,
        Some(serde_json::json!({"tool_name": "bash"})),
    )];
    write_tool_events(sid, &events);

    let outcome = recover_from_crash(TEST_USER_ID, sid).unwrap();
    match outcome {
        Some(RecoveryOutcome::RequiresUserInput {
            pending_decisions, ..
        }) => {
            assert!(
                !pending_decisions.is_empty(),
                "expected pending decisions for in-flight bash"
            );
            let has_bash = pending_decisions.iter().any(|(name, _)| name == "bash");
            assert!(
                has_bash,
                "expected bash in pending decisions, got {:?}",
                pending_decisions
            );
        }
        other => panic!(
            "expected RequiresUserInput, got {:?}",
            other.map(|o| format!("{:?}", o))
        ),
    }
}

// ── Requires user input: side-effect completed, no cache ───────────────────

#[test]
fn requires_user_input_side_effect_completed_no_cache() {
    let temp = tempfile::tempdir().unwrap();
    let _guard = JournalDirGuard::new(temp.path());
    let sid = "cr-int-side-effect-completed";

    write_test_heavy_checkpoint(sid, "session-turn-3-step-1", 1000);

    let events = vec![
        make_step_event(
            "ev-1",
            "session-turn-4-step-1",
            StepEventType::ToolCallStarted,
            2000,
            Some(serde_json::json!({"tool_name": "bash"})),
        ),
        make_step_event(
            "ev-2",
            "session-turn-4-step-1",
            StepEventType::ToolCallCompleted,
            2500,
            // No cached result — side-effect tool completed without cache → requires user input
            Some(serde_json::json!({"tool_name": "bash"})),
        ),
    ];
    write_tool_events(sid, &events);

    let outcome = recover_from_crash(TEST_USER_ID, sid).unwrap();
    // Side-effect tools that completed without cached results require user confirmation
    match outcome {
        Some(RecoveryOutcome::RequiresUserInput {
            pending_decisions, ..
        }) => {
            assert!(!pending_decisions.is_empty());
        }
        other => panic!(
            "expected RequiresUserInput, got {:?}",
            other.map(|o| format!("{:?}", o))
        ),
    }
}

// ── No crash: missing checkpoint ──────────────────────────────────────────

#[test]
fn no_checkpoint_returns_none() {
    let temp = tempfile::tempdir().unwrap();
    let _guard = JournalDirGuard::new(temp.path());
    let sid = "cr-int-no-checkpoint";

    let outcome = recover_from_crash(TEST_USER_ID, sid).unwrap();
    assert!(
        outcome.is_none(),
        "expected None when no checkpoint exists, got {:?}",
        outcome
    );
}

// ── Mixed tools: some safe, some need decision ─────────────────────────────

#[test]
fn mixed_tools_partial_auto_recover_blocked() {
    let temp = tempfile::tempdir().unwrap();
    let _guard = JournalDirGuard::new(temp.path());
    let sid = "cr-int-mixed";

    write_test_heavy_checkpoint(sid, "session-turn-1-step-1", 1000);

    let events = vec![
        // Pure read → safe
        make_step_event(
            "ev-1",
            "session-turn-2-step-1",
            StepEventType::ToolCallStarted,
            2000,
            Some(serde_json::json!({"tool_name": "read_file"})),
        ),
        make_step_event(
            "ev-2",
            "session-turn-2-step-1",
            StepEventType::ToolCallCompleted,
            2500,
            Some(serde_json::json!({"tool_name": "read_file", "result": "ok"})),
        ),
        // Side-effect tool in-flight → needs decision
        make_step_event(
            "ev-3",
            "session-turn-2-step-2",
            StepEventType::ToolCallStarted,
            3000,
            Some(serde_json::json!({"tool_name": "bash"})),
        ),
    ];
    write_tool_events(sid, &events);

    let outcome = recover_from_crash(TEST_USER_ID, sid).unwrap();
    match outcome {
        Some(RecoveryOutcome::RequiresUserInput {
            pending_decisions, ..
        }) => {
            // Only bash (in-flight side-effect) needs user input
            let decision_names: Vec<&str> =
                pending_decisions.iter().map(|(n, _)| n.as_str()).collect();
            assert!(decision_names.contains(&"bash"));
            assert!(
                !decision_names.contains(&"read_file"),
                "read_file should not need user decision"
            );
        }
        other => panic!(
            "expected RequiresUserInput, got {:?}",
            other.map(|o| format!("{:?}", o))
        ),
    }
}

// ── Failed tool → safe to replay ──────────────────────────────────────────

#[test]
fn failed_side_effect_tool_requires_user_input() {
    // Regression: Failed SideEffect tools (e.g. bash) may have partially executed
    // before the failure (e.g. "rm a/ b/ c/" deleted a/ before crashing).
    // Auto-replaying doubles the mutation. Must return RequiresUserInput.
    let temp = tempfile::tempdir().unwrap();
    let _guard = JournalDirGuard::new(temp.path());
    let sid = "test-failed-side-effect";

    write_test_heavy_checkpoint(sid, "session-turn-1-step-1", 1000);

    write_tool_events(
        sid,
        &[
            StepEvent {
                event_id: "ev-1".to_string(),
                run_id: "test-run".into(),
                canonical_event_id: None,
                step_id: "session-turn-2-step-1".to_string(),
                event_type: StepEventType::ToolCallStarted,
                agent_id: None,
                caused_by: vec![],
                payload: Some(serde_json::json!({"tool_name": "bash"})),
                created_at: 2000,
            },
            StepEvent {
                event_id: "ev-2".to_string(),
                run_id: "test-run".into(),
                canonical_event_id: None,
                step_id: "session-turn-2-step-1".to_string(),
                event_type: StepEventType::ToolCallFailed,
                agent_id: None,
                caused_by: vec![],
                payload: Some(serde_json::json!({"tool_name": "bash", "error": "exit 1"})),
                created_at: 2500,
            },
        ],
    );

    let outcome = recover_from_crash(TEST_USER_ID, sid).unwrap();
    // Failed SideEffect tool must NOT auto-recover — may have partial mutations.
    assert!(
        matches!(outcome, Some(RecoveryOutcome::RequiresUserInput { .. })),
        "failed SideEffect tool should require user input, got {:?}",
        outcome.map(|o| format!("{:?}", o))
    );
}

// ── In-flight tool classification: pure-read in-flight is safe ─────────────

#[test]
fn pure_read_in_flight_is_safe() {
    let temp = tempfile::tempdir().unwrap();
    let _guard = JournalDirGuard::new(temp.path());
    let sid = "cr-int-read-inflight";

    write_test_heavy_checkpoint(sid, "session-turn-1-step-1", 1000);

    let events = vec![make_step_event(
        "ev-1",
        "session-turn-2-step-1",
        StepEventType::ToolCallStarted,
        2000,
        Some(serde_json::json!({"tool_name": "grep"})),
    )];
    write_tool_events(sid, &events);

    let outcome = recover_from_crash(TEST_USER_ID, sid).unwrap();
    assert!(
        matches!(outcome, Some(RecoveryOutcome::AutoRecovered { .. })),
        "in-flight pure-read tools should auto-recover, got {:?}",
        outcome.map(|o| format!("{:?}", o))
    );
}

// ── Skipped tool: already skipped during run ──────────────────────────────

#[test]
fn skipped_tool_auto_recovers() {
    let temp = tempfile::tempdir().unwrap();
    let _guard = JournalDirGuard::new(temp.path());
    let sid = "cr-int-skipped";

    write_test_heavy_checkpoint(sid, "session-turn-1-step-1", 1000);

    let events = vec![
        make_step_event(
            "ev-1",
            "session-turn-2-step-1",
            StepEventType::ToolCallStarted,
            2000,
            Some(serde_json::json!({"tool_name": "bash"})),
        ),
        make_step_event(
            "ev-2",
            "session-turn-2-step-1",
            StepEventType::ToolCallSkipped,
            2500,
            Some(serde_json::json!({"tool_name": "bash"})),
        ),
    ];
    write_tool_events(sid, &events);

    let outcome = recover_from_crash(TEST_USER_ID, sid).unwrap();
    // Skipped tools are ignored → auto-recover
    assert!(
        matches!(outcome, Some(RecoveryOutcome::AutoRecovered { .. })),
        "skipped tools should auto-recover, got {:?}",
        outcome.map(|o| format!("{:?}", o))
    );
}
