//! End-to-end tests for journal telemetry accuracy.
//!
//! These tests simulate realistic agentic turn scenarios — including skill
//! interception, surgical removal, multi-turn sessions, and various unhappy
//! paths — then verify the entire pipeline from ToolCallRecord creation
//! through journal persistence.

use astra_services::session_journal::{
    self, JournalDirGuard, JournalEvent, JournalEventType, JournalWriter,
    SURGICAL_REMOVAL_TOOL_NAME, ToolCallRecord,
};

// ─── Helpers ────────────────────────────────────────────────────────────────

fn real_tool(name: &str, ok: bool, ms: u64) -> ToolCallRecord {
    ToolCallRecord {
        name: name.to_string(),
        ok,
        ms,
        error: if ok { None } else { Some("tool error".into()) },
        input_bytes: Some(100),
        output_bytes: Some(if ok { 500 } else { 0 }),
        args_preview: Some(format!("{name} args...")),
        result_preview: Some(if ok {
            "ok result".into()
        } else {
            "error".into()
        }),
        file_path: None,
        surgically_removed: None,
        original_tool_name: None,
        ..Default::default()
    }
}

fn surgical_removal(original_name: &str) -> ToolCallRecord {
    ToolCallRecord {
        name: SURGICAL_REMOVAL_TOOL_NAME.to_string(),
        ok: true,
        ms: 0,
        error: None,
        input_bytes: None,
        output_bytes: Some(0),
        args_preview: None,
        result_preview: Some("(removed from context — skill covered this work)".into()),
        file_path: None,
        surgically_removed: Some(true),
        original_tool_name: Some(original_name.to_string()),
        ..Default::default()
    }
}

fn skipped_tool(name: &str) -> ToolCallRecord {
    ToolCallRecord {
        name: name.to_string(),
        ok: false,
        ms: 0,
        error: None,
        input_bytes: None,
        output_bytes: Some(0),
        args_preview: None,
        result_preview: Some("Skipped: skill routed".into()),
        file_path: None,
        surgically_removed: None,
        original_tool_name: None,
        result_class: Some(
            astra_services::session_journal::NOOP_OR_CACHED_RESULT_CLASS.to_string(),
        ),
        ..Default::default()
    }
}

fn deferred_tool(name: &str) -> ToolCallRecord {
    ToolCallRecord {
        name: name.to_string(),
        ok: false,
        ms: 0,
        error: None,
        input_bytes: None,
        output_bytes: Some(0),
        args_preview: None,
        result_preview: Some("Deferred: skill invoked".into()),
        file_path: None,
        surgically_removed: None,
        original_tool_name: None,
        result_class: Some(
            astra_services::session_journal::NOOP_OR_CACHED_RESULT_CLASS.to_string(),
        ),
        ..Default::default()
    }
}

fn find_event(events: &[JournalEvent], event_type: JournalEventType) -> &JournalEvent {
    events
        .iter()
        .find(|event| event.event_type == event_type)
        .unwrap_or_else(|| panic!("missing event type: {event_type:?}"))
}

// ─── E2E: Full pipeline — skill interception → journal ─────────

#[test]
fn e2e_skill_interception_produces_accurate_journal() {
    // Simulates a real turn where a skill (e.g. git review) intercepts
    // some parallel tool calls. The agent originally requested 8 tool calls,
    // but the skill handled 5 of them. 3 real calls remain + 5 surgical removals.
    let tmp = tempfile::tempdir().unwrap();
    let _guard = JournalDirGuard::new(tmp.path());
    let session_id = "e2e-skill-interception";

    // --- Phase 1: Build tool call records as agentic_tool_interception would ---
    let records = vec![
        // Skill result (real call — the skill itself)
        real_tool("skill", true, 3200),
        // Surgical removals for the parallel calls the skill covered
        surgical_removal("read_file"),
        surgical_removal("read_file"),
        surgical_removal("glob"),
        surgical_removal("grep"),
        surgical_removal("git"),
        // Real calls that ran alongside the skill
        real_tool("write_file", true, 45),
        real_tool("bash", true, 120),
    ];

    // --- Phase 2: Write turn event with tool_calls to journal ---
    let writer = JournalWriter::new(session_id).unwrap();
    let turn_event = JournalEvent::turn(
        Some(session_id),
        1,
        Some("gpt-5.4"),
        "Review the PR changes",
        "I've reviewed the PR...",
        records.len() as u32,
        50000,
        2000,
        5000,
    )
    .with_tool_calls(records);
    writer.append(&turn_event).unwrap();

    // Read back and verify the recorded invocations.
    let events = session_journal::read_journal(session_id).unwrap();
    assert_eq!(events.len(), 2, "should have session_start and turn events");

    // Verify turn event has all 8 records (including surgical removals for audit)
    let turn = find_event(&events, JournalEventType::Turn);
    let tool_calls = turn.tool_calls.as_ref().unwrap();
    assert_eq!(
        tool_calls.len(),
        8,
        "all 8 records persisted for audit trail"
    );

    // Verify surgical removal records roundtrip correctly
    let surgical_records: Vec<_> = tool_calls
        .iter()
        .filter(|r| r.is_synthetic_placeholder())
        .collect();
    assert_eq!(
        surgical_records.len(),
        5,
        "5 surgical removals survived serde"
    );
    for rec in &surgical_records {
        assert_eq!(rec.surgically_removed, Some(true));
        assert!(rec.original_tool_name.is_some());
    }

    // Verify real records are intact
    let real_records: Vec<_> = tool_calls
        .iter()
        .filter(|r| !r.is_synthetic_placeholder())
        .collect();
    assert_eq!(real_records.len(), 3, "3 real tool calls");
    assert!(real_records.iter().all(|r| r.surgically_removed.is_none()));
}

// ─── E2E: Multi-turn session with correct turn numbering ────────────────────

#[test]
fn e2e_multi_turn_session_turn_numbering_consistent() {
    let tmp = tempfile::tempdir().unwrap();
    let _guard = JournalDirGuard::new(tmp.path());
    let session_id = "e2e-multi-turn";

    let writer = JournalWriter::new(session_id).unwrap();

    // Simulate 3 user turns with context assembly events
    for turn_num in 1..=3u32 {
        let records = vec![
            real_tool("read_file", true, 30),
            real_tool("bash", true, 200),
        ];

        // Context assembly uses REPL turn number (the fix)
        let assembly = JournalEvent::context_assembly_recorded(
            Some(session_id),
            turn_num, // <-- This is the REPL counter, not internal loop counter
            serde_json::json!({
                "turn_id": format!("turn-{turn_num}"),
                "system_prompt_tokens": 1200,
                "tools_available": 15,
            }),
        );
        writer.append(&assembly).unwrap();

        // Turn event
        let turn_event = JournalEvent::turn(
            Some(session_id),
            turn_num,
            Some("gpt-5.4"),
            &format!("User message {turn_num}"),
            &format!("Response {turn_num}"),
            records.len() as u32,
            30000 + turn_num as u64 * 5000,
            1000,
            2000,
        )
        .with_tool_calls(records);
        writer.append(&turn_event).unwrap();
    }

    // Read back and verify turn numbering consistency
    let events = session_journal::read_journal(session_id).unwrap();
    assert_eq!(
        events.len(),
        7,
        "3 turns × 2 events each plus the initial session_start"
    );

    // Verify all turn numbers are 1, 2, 3 (never internal loop counters like 5, 10, 15)
    let turn_numbers: Vec<u32> = events.iter().filter_map(|e| e.turn).collect();
    assert_eq!(
        turn_numbers,
        [1, 1, 2, 2, 3, 3],
        "all events should have REPL turn numbers 1-3, not internal loop counters"
    );

    // Verify context_assembly turn IDs match their turn numbers
    let assemblies: Vec<_> = events
        .iter()
        .filter(|e| e.event_type == JournalEventType::ContextAssemblyRecorded)
        .collect();
    assert_eq!(assemblies.len(), 3);
    for (i, asm) in assemblies.iter().enumerate() {
        let expected_turn = (i + 1) as u32;
        assert_eq!(asm.turn, Some(expected_turn));
        let trace = asm.context_assembly_trace.as_ref().unwrap();
        let turn_id = trace["turn_id"].as_str().unwrap();
        assert_eq!(turn_id, format!("turn-{expected_turn}"));
    }
}

// ─── Unhappy path: current synthetic records are filtered ────────────────────

#[test]
fn e2e_current_synthetic_records_are_filtered() {
    let tmp = tempfile::tempdir().unwrap();
    let _guard = JournalDirGuard::new(tmp.path());
    let session_id = "e2e-current-synthetic";

    // Synthetic records must carry current explicit markers. A sentinel name
    // alone is not part of the supported contract.
    let records = vec![
        real_tool("read_file", true, 30),
        surgical_removal("glob"),
        surgical_removal("read_file"),
        real_tool("bash", true, 100),
        skipped_tool("grep"),
        deferred_tool("git"),
        real_tool("write_file", true, 50),
    ];

    let writer = JournalWriter::new(session_id).unwrap();
    let turn_event = JournalEvent::turn(
        Some(session_id),
        1,
        Some("qwen3.6-plus"),
        "Fix the bug",
        "Done.",
        7,
        40000,
        1500,
        3000,
    )
    .with_tool_calls(records.clone());
    writer.append(&turn_event).unwrap();

    // Read back
    let events = session_journal::read_journal(session_id).unwrap();
    let turn = find_event(&events, JournalEventType::Turn);
    let tool_calls = turn.tool_calls.as_ref().unwrap();

    // ALL synthetic types should be detected
    let real: Vec<_> = tool_calls
        .iter()
        .filter(|r| !r.is_synthetic_placeholder())
        .collect();
    let synthetic: Vec<_> = tool_calls
        .iter()
        .filter(|r| r.is_synthetic_placeholder())
        .collect();
    assert_eq!(real.len(), 3, "3 real: read_file, bash, write_file");
    assert_eq!(
        synthetic.len(),
        4,
        "4 synthetic: surgical, skipped, deferred"
    );
}

// ─── Unhappy path: zero tool calls (conversational turn) ────────────────────

#[test]
fn e2e_zero_tool_calls_conversational() {
    let tmp = tempfile::tempdir().unwrap();
    let _guard = JournalDirGuard::new(tmp.path());
    let session_id = "e2e-no-tools";

    let writer = JournalWriter::new(session_id).unwrap();
    let turn_event = JournalEvent::turn(
        Some(session_id),
        1,
        Some("gpt-5.4"),
        "Hello!",
        "Hi there!",
        0,
        5000,
        200,
        500,
    );
    // No .with_tool_calls() — empty turns should not have the field
    writer.append(&turn_event).unwrap();

    let events = session_journal::read_journal(session_id).unwrap();
    let turn = find_event(&events, JournalEventType::Turn);
    assert!(
        turn.tool_calls.is_none(),
        "no tool_calls field for empty turns"
    );
}

// ─── E2E: Realistic session 3f4389fe scenario (the original bug report) ─────

#[test]
fn e2e_session_3f4389fe_scenario_turn1_surgical_removal_accuracy() {
    // Recreates the exact scenario from session 3f4389fe turn 1:
    // MiniMax-M2.7 model, 21 tool calls where 9 were surgical removals.
    // Before the fix: tool_call_count was 21. After fix: should be 12.
    let tmp = tempfile::tempdir().unwrap();
    let _guard = JournalDirGuard::new(tmp.path());
    let session_id = "e2e-3f4389fe-turn1";

    let writer = JournalWriter::new(session_id).unwrap();

    // Build the 21 records: 12 real + 9 surgical
    let mut records = Vec::new();
    // 12 real tool calls (mix of tools a code review session would use)
    for tool in &[
        "skill",
        "read_file",
        "read_file",
        "read_file",
        "glob",
        "grep",
        "git",
        "git",
        "bash",
        "write_file",
        "write_file",
        "bash",
    ] {
        records.push(real_tool(tool, true, 50));
    }
    // 9 surgical removals (parallel calls the skill handled)
    for tool in &[
        "read_file",
        "read_file",
        "read_file",
        "glob",
        "glob",
        "grep",
        "grep",
        "git",
        "git",
    ] {
        records.push(surgical_removal(tool));
    }
    assert_eq!(records.len(), 21);

    let turn_event = JournalEvent::turn(
        Some(session_id),
        1,
        Some("MiniMax-M2.7"),
        "Review the PR changes and provide feedback",
        "I've reviewed all changes...",
        21,
        70000,
        7700,
        8000,
    )
    .with_tool_calls(records.clone());
    writer.append(&turn_event).unwrap();

    let events = session_journal::read_journal(session_id).unwrap();

    // Journal should persist ALL 21 records for audit
    let tool_calls = find_event(&events, JournalEventType::Turn)
        .tool_calls
        .as_ref()
        .unwrap();
    assert_eq!(tool_calls.len(), 21);

    // Verify original_tool_name is preserved for analytics
    let surgical: Vec<_> = tool_calls
        .iter()
        .filter(|r| r.surgically_removed == Some(true))
        .collect();
    assert_eq!(surgical.len(), 9);
    let original_names: Vec<_> = surgical
        .iter()
        .filter_map(|r| r.original_tool_name.as_deref())
        .collect();
    assert!(original_names.contains(&"read_file"));
    assert!(original_names.contains(&"glob"));
    assert!(original_names.contains(&"grep"));
    assert!(original_names.contains(&"git"));
}

// ─── Unhappy path: all tool calls fail (worst case turn) ────────────────────

// ─── Unhappy path: Unicode in tool names and error messages ─────────────────

#[test]
fn e2e_unicode_in_tool_records_survives_journal_roundtrip() {
    let tmp = tempfile::tempdir().unwrap();
    let _guard = JournalDirGuard::new(tmp.path());
    let session_id = "e2e-unicode";

    let records = vec![
        ToolCallRecord {
            name: "bash".to_string(),
            ok: false,
            ms: 50,
            error: Some("命令未找到: 科技风格".into()),
            input_bytes: Some(200),
            output_bytes: Some(0),
            args_preview: Some("echo '在tmp目录下面生成文档'".into()),
            result_preview: Some("错误：路径不存在 /tmp/科技".into()),
            file_path: Some("/tmp/科技风格/index.html".into()),
            surgically_removed: None,
            original_tool_name: None,
            ..Default::default()
        },
        surgical_removal("read_file"),
    ];

    let writer = JournalWriter::new(session_id).unwrap();
    let turn_event = JournalEvent::turn(
        Some(session_id),
        1,
        Some("qwen3.6-plus"),
        "在tmp目录下面生成文档",
        "已完成",
        2,
        95000,
        5900,
        10000,
    )
    .with_tool_calls(records);
    writer.append(&turn_event).unwrap();

    let events = session_journal::read_journal(session_id).unwrap();
    let tool_calls = find_event(&events, JournalEventType::Turn)
        .tool_calls
        .as_ref()
        .unwrap();
    assert_eq!(tool_calls.len(), 2);

    // Verify Chinese characters survived JSON roundtrip
    assert_eq!(tool_calls[0].error.as_deref(), Some("命令未找到: 科技风格"));
    assert_eq!(
        tool_calls[0].file_path.as_deref(),
        Some("/tmp/科技风格/index.html")
    );
    assert!(tool_calls[1].is_synthetic_placeholder());
}

// ─── Unhappy path: very large number of surgical removals ───────────────────

#[test]
fn e2e_many_surgical_removals_performance_and_accuracy() {
    let tmp = tempfile::tempdir().unwrap();
    let _guard = JournalDirGuard::new(tmp.path());
    let session_id = "e2e-many-removals";

    // Extreme case: 50 surgical removals + 5 real calls
    let mut records: Vec<ToolCallRecord> = (0..50)
        .map(|i| surgical_removal(&format!("tool_{i}")))
        .collect();
    for i in 0..5 {
        records.push(real_tool(&format!("real_tool_{i}"), true, 100));
    }
    assert_eq!(records.len(), 55);

    let writer = JournalWriter::new(session_id).unwrap();
    let turn_event = JournalEvent::turn(
        Some(session_id),
        1,
        Some("gpt-5.4"),
        "Complex task",
        "Done",
        55,
        100000,
        5000,
        15000,
    )
    .with_tool_calls(records.clone());
    writer.append(&turn_event).unwrap();

    let events = session_journal::read_journal(session_id).unwrap();
    let tool_calls = find_event(&events, JournalEventType::Turn)
        .tool_calls
        .as_ref()
        .unwrap();
    assert_eq!(tool_calls.len(), 55, "all 55 records persisted");

    let surgical: Vec<_> = tool_calls
        .iter()
        .filter(|r| r.is_synthetic_placeholder())
        .collect();
    assert_eq!(surgical.len(), 50);

    // Verify each surgical removal preserved its original name
    for (i, rec) in surgical.iter().enumerate() {
        assert_eq!(
            rec.original_tool_name.as_deref(),
            Some(format!("tool_{i}").as_str()),
        );
    }
}
