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
fn test_heavy_checkpoint(step_id: &str, created_at: u64) -> HeavyCheckpoint {
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

    HeavyCheckpoint {
        light,
        conversation_cursor: None,
        messages: vec![],
        budget_remaining_tokens: 0,
        budget_remaining_rounds: 0,
        run_execution_budget: None,
        run_execution_control: None,
        blocked_tools: vec![],
        recent_tools: vec![],
        deferred_tool_activations: vec![],
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
    }
}

fn write_test_heavy_checkpoint(session_id: &str, step_id: &str, created_at: u64) {
    write_heavy_checkpoint(session_id, test_heavy_checkpoint(step_id, created_at));
}

fn write_heavy_checkpoint(session_id: &str, heavy: HeavyCheckpoint) {
    write_step_checkpoint(
        TEST_USER_ID,
        session_id,
        1,
        &StepCheckpoint::Heavy(Box::new(heavy)),
    )
    .unwrap();
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

#[test]
fn recovery_rejects_cursor_outside_the_checkpoint_session_or_root() {
    let temp = tempfile::tempdir().unwrap();
    let _guard = JournalDirGuard::new(temp.path());
    for (sid, cursor_sid, root) in [
        ("cr-wrong-session", "foreign-session", None),
        ("cr-wrong-root", "cr-wrong-root", Some("wrong-root")),
    ] {
        let mut heavy = test_heavy_checkpoint("session-turn-3", 1000);
        heavy.messages = vec![serde_json::json!({"role":"user","content":"resume"})];
        heavy.conversation_cursor = Some(astra_turn_types::SessionCursorV1 {
            schema_version: astra_turn_types::SESSION_CURSOR_SCHEMA_VERSION,
            owner_id: TEST_USER_ID.into(),
            session_id: cursor_sid.into(),
            branch_id: astra_turn_types::DEFAULT_CONVERSATION_BRANCH_ID.into(),
            completed_turn: 3,
            journal_event_seq: 3,
            conversation_seq: 3,
            canonical_root_hash: root
                .map(str::to_string)
                .unwrap_or_else(|| astra_turn_types::canonical_conversation_root(&heavy.messages)),
            projection_schema: astra_turn_types::CONVERSATION_PROJECTION_SCHEMA_VERSION,
            compaction_generation: 0,
            config_version_id: None,
        });
        write_heavy_checkpoint(sid, heavy);
        let error = recover_from_crash(TEST_USER_ID, sid).unwrap_err();
        assert!(
            matches!(
                error,
                astra_pipeline::crash_recovery::RecoveryError::CorruptedCheckpoint(_)
            ),
            "{sid}: {error:?}"
        );
    }
}

#[test]
fn recovery_rejects_invalid_execution_cursors() {
    use astra_pipeline::step_protocol::{SlotState, StepAction};
    let temp = tempfile::tempdir().unwrap();
    let _guard = JournalDirGuard::new(temp.path());
    let act = ExecutionCursor {
        phase: StepAction::Act,
        ..Default::default()
    };
    let wait = ExecutionCursor {
        phase: StepAction::Wait,
        ..Default::default()
    };
    let mut running = ExecutionCursor::for_act(1);
    running.slots[0].state = SlotState::Running;
    for (sid, cursor) in [
        ("cr-empty-act", act),
        ("cr-empty-wait", wait),
        ("cr-running-slot", running),
    ] {
        let mut heavy = test_heavy_checkpoint("session-turn-3", 1000);
        heavy.light.cursor = cursor;
        write_heavy_checkpoint(sid, heavy);
        assert!(recover_from_crash(TEST_USER_ID, sid).is_err(), "{sid}");
        // The ordinary checkpoint projection uses the same validator.
        assert!(
            astra_pipeline::step_restore::restore_session(TEST_USER_ID, sid).is_err(),
            "{sid}"
        );
    }
}

#[test]
fn recovery_rejects_unpaired_or_foreign_execution_control() {
    use astra_pipeline::step_protocol::{RunExecutionBudget, RunExecutionControl};
    let temp = tempfile::tempdir().unwrap();
    let _guard = JournalDirGuard::new(temp.path());
    for (sid, budget) in [
        ("cr-missing-budget", None),
        (
            "cr-wrong-budget-owner",
            Some(RunExecutionBudget::V1 {
                run_id: "foreign-run".into(),
                producer_owner_generation: 3,
                charged_iterations: 2,
                granted_iteration_boundary: 20,
                remaining_iterations: 18,
                effective_hard_turn_limit: None,
            }),
        ),
    ] {
        let mut heavy = test_heavy_checkpoint("session-turn-3", 1000);
        heavy.run_execution_budget = budget;
        heavy.run_execution_control = Some(RunExecutionControl::V4 {
            completion_settlement: Default::default(),
            hook_obligations: Default::default(),
            reply_obligations: astra_turn_types::ReplyObligationsSnapshotV1 {
                run_id: "control-run".into(),
                producer_owner_generation: 3,
                pending: Vec::new(),
            },
            budget_wrapup_injected: false,
            budget_wrapup_ignored_rounds: 0,
        });
        write_heavy_checkpoint(sid, heavy);
        assert!(recover_from_crash(TEST_USER_ID, sid).is_err(), "{sid}");
    }
}

#[test]
fn damaged_journal_cannot_be_accepted_as_unknown_tool_effects() {
    let temp = tempfile::tempdir().unwrap();
    let _guard = JournalDirGuard::new(temp.path());
    let sid = "cr-gap-with-pending";
    write_test_heavy_checkpoint(sid, "session-turn-3", 1000);
    write_tool_events(
        sid,
        &[
            make_step_event(
                "start",
                "step-1",
                StepEventType::ToolCallStarted,
                2000,
                Some(serde_json::json!({"tool_name":"bash","call_id":"pending"})),
            ),
            make_step_event(
                "later",
                "step-2",
                StepEventType::StepCompleted,
                400_000,
                None,
            ),
        ],
    );
    assert!(matches!(
        recover_from_crash(TEST_USER_ID, sid),
        Err(astra_pipeline::crash_recovery::RecoveryError::JournalGap { .. })
    ));
}

#[test]
fn recovery_retains_completed_output_as_audit_without_cache_authority() {
    let temp = tempfile::tempdir().unwrap();
    let _guard = JournalDirGuard::new(temp.path());
    let sid = "cr-audit-only";
    write_test_heavy_checkpoint(sid, "session-turn-3", 1000);
    let key = astra_pipeline::step_protocol::IdempotencyKey::semantic(
        "read_file",
        &serde_json::json!({"path":"file"}),
    );
    write_tool_events(
        sid,
        &[
            make_step_event(
                "old",
                "step-old",
                StepEventType::ToolCallCompleted,
                500,
                Some(serde_json::json!({"tool_name":"read_file","output":"before checkpoint"})),
            ),
            make_step_event(
                "result",
                "step-1",
                StepEventType::ToolCallCompleted,
                2000,
                Some(
                    serde_json::json!({"tool_name":"read_file","result":"result field","idempotency_key":key.cache_key()}),
                ),
            ),
            make_step_event(
                "output",
                "step-2",
                StepEventType::ToolCallCompleted,
                3000,
                Some(
                    serde_json::json!({"tool_name":"read_file","output":"output field","idempotency_key":"semantic:freshness=sha256:unverified"}),
                ),
            ),
        ],
    );
    let Some(RecoveryOutcome::AutoRecovered { restored }) =
        recover_from_crash(TEST_USER_ID, sid).unwrap()
    else {
        panic!("completed results need no uncertainty confirmation");
    };
    assert_eq!(
        restored.completed_tool_results["read_file"],
        ["result field", "output field"]
    );
    assert_eq!(restored.cache_restore_report.rejected_unverified_entries, 2);
    assert_eq!(
        restored.cache_restore_report.rejected_context_bound_entries,
        1
    );
    assert_eq!(restored.cache_restore_report.events_examined, 2);
    assert_eq!(restored.resume_turn, 3);
}

#[test]
fn recovery_does_not_read_another_owner_checkpoint() {
    let temp = tempfile::tempdir().unwrap();
    let _guard = JournalDirGuard::new(temp.path());
    let sid = "cr-owner-scoped";
    write_test_heavy_checkpoint(sid, "session-turn-3", 1000);
    assert!(recover_from_crash("foreign-owner", sid).unwrap().is_none());
    assert!(recover_from_crash(TEST_USER_ID, sid).unwrap().is_some());
}

#[test]
fn recovery_rejects_invalid_quarantine_and_checkpoint_version() {
    let temp = tempfile::tempdir().unwrap();
    let _guard = JournalDirGuard::new(temp.path());
    let mut quarantine = test_heavy_checkpoint("session-turn-3", 1000);
    quarantine.workspace_observation_quarantine = Some(
        astra_pipeline::step_protocol::WorkspaceObservationQuarantineV1 {
            reason: "unsupported_reason".into(),
            scope: "bound_workspace".into(),
            source_tool_call_id: None,
        },
    );
    let mut version = test_heavy_checkpoint("session-turn-3", 1000);
    version.light.protocol_version += 1;
    for (sid, heavy) in [
        ("cr-invalid-quarantine", quarantine),
        ("cr-invalid-version", version),
    ] {
        write_heavy_checkpoint(sid, heavy);
        assert!(recover_from_crash(TEST_USER_ID, sid).is_err(), "{sid}");
    }
}

#[test]
fn recovery_rejects_a_torn_tool_receipt() {
    use astra_services::{OwnerScope, SessionArtifactStore};
    use std::io::Write;
    let temp = tempfile::tempdir().unwrap();
    let _guard = JournalDirGuard::new(temp.path());
    let sid = "cr-torn-receipt";
    write_test_heavy_checkpoint(sid, "session-turn-3", 1000);
    write_tool_events(
        sid,
        &[make_step_event(
            "start",
            "step-1",
            StepEventType::ToolCallStarted,
            2000,
            Some(serde_json::json!({"tool_name":"bash","call_id":"pending"})),
        )],
    );
    let owner = OwnerScope::user(TEST_USER_ID).unwrap();
    let path = astra_services::local_session_artifact_store()
        .session_dir_for_owner(&owner, sid)
        .unwrap()
        .join("step_events.jsonl");
    std::fs::OpenOptions::new()
        .append(true)
        .open(path)
        .unwrap()
        .write_all(b"{torn receipt")
        .unwrap();
    assert!(matches!(
        recover_from_crash(TEST_USER_ID, sid),
        Err(astra_pipeline::crash_recovery::RecoveryError::JournalRead(
            _
        ))
    ));
}
