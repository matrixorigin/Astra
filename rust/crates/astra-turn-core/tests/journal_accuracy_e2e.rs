//! End-to-end tests for journal telemetry accuracy.
//!
//! These tests simulate realistic agentic turn scenarios — including skill
//! interception, surgical removal, multi-turn sessions, and various unhappy
//! paths — then verify the entire pipeline from ToolCallRecord creation
//! through journal persistence to turn evaluation.

use astra_services::session_journal::{
    self, JournalDirGuard, JournalEvent, JournalEventType, JournalWriter,
    SURGICAL_REMOVAL_TOOL_NAME, ToolCallRecord,
};
use astra_turn_core::evaluation::build_turn_evaluation_journal_event;

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

fn legacy_surgical_removal() -> ToolCallRecord {
    // Old-format record: no surgically_removed flag, just the sentinel name
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
        surgically_removed: None,
        original_tool_name: None,
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
        ..Default::default()
    }
}

fn make_eval(success: bool, quality: f64) -> astra_turn_core::evaluation::TurnEvaluation {
    astra_turn_core::evaluation::TurnEvaluation {
        success,
        quality,
        confidence: 0.8,
        signals: vec![],
    }
}

fn extract_tool_call_count(event: &JournalEvent) -> usize {
    event
        .metadata
        .as_ref()
        .and_then(|m| m.get("tool_call_count"))
        .and_then(|v| v.as_u64())
        .unwrap_or(0) as usize
}

// ─── E2E: Full pipeline — skill interception → journal → evaluation ─────────

#[test]
fn e2e_skill_interception_produces_accurate_journal_and_evaluation() {
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
        surgical_removal("git_show"),
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
    .with_tool_calls(records.clone());
    writer.append(&turn_event).unwrap();

    // --- Phase 3: Build evaluation (as evaluation.rs would) ---
    let eval_event = build_turn_evaluation_journal_event(
        Some(session_id),
        Some(1),
        "gpt-5.4",
        "Review the PR changes",
        &["skill".into(), "write_file".into(), "bash".into()],
        &records,
        0,
        false,
        0.2,
        &make_eval(true, 0.85),
    );
    writer.append(&eval_event).unwrap();

    // --- Phase 4: Read back and verify ---
    let events = session_journal::read_journal(session_id).unwrap();
    assert_eq!(events.len(), 2, "should have turn + evaluation events");

    // Verify turn event has all 8 records (including surgical removals for audit)
    let turn = &events[0];
    assert_eq!(turn.event_type, JournalEventType::Turn);
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

    // Verify evaluation event has correct tool_call_count (EXCLUDES synthetics)
    let eval = &events[1];
    assert_eq!(eval.event_type, JournalEventType::TurnEvaluation);
    let tool_call_count = extract_tool_call_count(eval);
    assert_eq!(
        tool_call_count, 3,
        "tool_call_count must be 3 (only real calls), not 8"
    );
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
        .with_tool_calls(records.clone());
        writer.append(&turn_event).unwrap();

        // Turn evaluation
        let eval = build_turn_evaluation_journal_event(
            Some(session_id),
            Some(turn_num),
            "gpt-5.4",
            &format!("User message {turn_num}"),
            &["read_file".into(), "bash".into()],
            &records,
            0,
            false,
            0.1 * turn_num as f64,
            &make_eval(true, 0.9 - turn_num as f64 * 0.1),
        );
        writer.append(&eval).unwrap();
    }

    // Read back and verify turn numbering consistency
    let events = session_journal::read_journal(session_id).unwrap();
    assert_eq!(events.len(), 9, "3 turns × 3 events each");

    // Verify all turn numbers are 1, 2, 3 (never internal loop counters like 5, 10, 15)
    let turn_numbers: Vec<u32> = events.iter().filter_map(|e| e.turn).collect();
    assert_eq!(
        turn_numbers,
        vec![1, 1, 1, 2, 2, 2, 3, 3, 3],
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

// ─── Unhappy path: mixed legacy + new surgical removal records ──────────────

#[test]
fn e2e_mixed_legacy_and_new_surgical_records_both_filtered() {
    let tmp = tempfile::tempdir().unwrap();
    let _guard = JournalDirGuard::new(tmp.path());
    let session_id = "e2e-mixed-legacy";

    // Simulate a scenario where journal has been migrated mid-session:
    // some records use old sentinel-name-only, others use new flag.
    let records = vec![
        real_tool("read_file", true, 30),
        legacy_surgical_removal(), // old format: no flag, just name
        surgical_removal("glob"),  // new format: flag + original_tool_name
        real_tool("bash", true, 100),
        skipped_tool("grep"),      // skill-routed skip
        deferred_tool("git_show"), // skill-routed defer
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

    // Build evaluation
    let eval_event = build_turn_evaluation_journal_event(
        Some(session_id),
        Some(1),
        "qwen3.6-plus",
        "Fix the bug",
        &[],
        &records,
        0,
        false,
        0.3,
        &make_eval(true, 0.7),
    );
    writer.append(&eval_event).unwrap();

    // Read back
    let events = session_journal::read_journal(session_id).unwrap();
    let turn = &events[0];
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
        "4 synthetic: legacy surgical, new surgical, skipped, deferred"
    );

    // tool_call_count in evaluation should match real count
    let eval = &events[1];
    assert_eq!(extract_tool_call_count(eval), 3);
}

// ─── Unhappy path: ALL records are surgical removals (edge case) ────────────

#[test]
fn e2e_all_surgical_removals_zero_real_tool_count() {
    let tmp = tempfile::tempdir().unwrap();
    let _guard = JournalDirGuard::new(tmp.path());
    let session_id = "e2e-all-surgical";

    // Edge case: skill handled everything, all parallel calls were removed
    let records = vec![
        surgical_removal("read_file"),
        surgical_removal("glob"),
        surgical_removal("grep"),
        surgical_removal("git_show"),
    ];

    let writer = JournalWriter::new(session_id).unwrap();
    let turn_event = JournalEvent::turn(
        Some(session_id),
        1,
        Some("gpt-5.4"),
        "Review code",
        "Reviewed.",
        4,
        20000,
        500,
        1000,
    )
    .with_tool_calls(records.clone());
    writer.append(&turn_event).unwrap();

    let eval_event = build_turn_evaluation_journal_event(
        Some(session_id),
        Some(1),
        "gpt-5.4",
        "Review code",
        &[],
        &records,
        0,
        false,
        0.1,
        &make_eval(true, 0.5),
    );
    writer.append(&eval_event).unwrap();

    let events = session_journal::read_journal(session_id).unwrap();
    let eval = &events[1];

    // tool_call_count should be 0, not 4
    assert_eq!(
        extract_tool_call_count(eval),
        0,
        "all surgical removals → tool_call_count must be 0"
    );
}

// ─── Unhappy path: zero tool calls (conversational turn) ────────────────────

#[test]
fn e2e_zero_tool_calls_conversational() {
    let tmp = tempfile::tempdir().unwrap();
    let _guard = JournalDirGuard::new(tmp.path());
    let session_id = "e2e-no-tools";

    let records: Vec<ToolCallRecord> = vec![];

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

    let eval_event = build_turn_evaluation_journal_event(
        Some(session_id),
        Some(1),
        "gpt-5.4",
        "Hello!",
        &[],
        &records,
        0,
        false,
        0.0,
        &make_eval(true, 0.6),
    );
    writer.append(&eval_event).unwrap();

    let events = session_journal::read_journal(session_id).unwrap();
    let turn = &events[0];
    assert!(
        turn.tool_calls.is_none(),
        "no tool_calls field for empty turns"
    );
    assert_eq!(extract_tool_call_count(&events[1]), 0);
}

// ─── Unhappy path: backward compat — deserialize legacy JSONL without new fields

#[test]
fn e2e_legacy_journal_without_surgical_fields_deserializes() {
    let tmp = tempfile::tempdir().unwrap();
    let _guard = JournalDirGuard::new(tmp.path());
    let session_id = "e2e-legacy-compat";

    // Write raw JSONL as it would appear from an old version (no surgical fields)
    let legacy_turn = serde_json::json!({
        "type": "turn",
        "ts": "2026-04-17T07:20:00Z",
        "session_id": session_id,
        "turn": 1,
        "model": "MiniMax-M2.7",
        "user_input": "Check the files",
        "assistant_output": "Found issues.",
        "tool_count": 3,
        "tokens_in": 70000,
        "tokens_out": 7700,
        "duration_ms": 8000,
        "tool_calls": [
            {
                "name": "read_file",
                "ok": true,
                "ms": 30,
                "output_bytes": 500
            },
            {
                "name": "(surgically_removed)",
                "ok": true,
                "ms": 0,
                "output_bytes": 0,
                "result_preview": "(removed from context — skill covered this work)"
            },
            {
                "name": "bash",
                "ok": false,
                "ms": 100,
                "error": "command not found"
            }
        ]
    });

    let path = tmp.path().join(format!("{session_id}.jsonl"));
    std::fs::write(&path, format!("{}\n", legacy_turn)).unwrap();

    let events = session_journal::read_journal(session_id).unwrap();
    assert_eq!(events.len(), 1);

    let tool_calls = events[0].tool_calls.as_ref().unwrap();
    assert_eq!(tool_calls.len(), 3);

    // Legacy surgical removal should still be detected as synthetic
    assert!(tool_calls[1].is_synthetic_placeholder());
    // surgically_removed field should be None (not in legacy JSON)
    assert_eq!(tool_calls[1].surgically_removed, None);
    assert_eq!(tool_calls[1].original_tool_name, None);

    // Real calls should not be synthetic
    assert!(!tool_calls[0].is_synthetic_placeholder());
    assert!(!tool_calls[2].is_synthetic_placeholder());

    // Build evaluation using the deserialized records — should count correctly
    let eval_event = build_turn_evaluation_journal_event(
        Some(session_id),
        Some(1),
        "MiniMax-M2.7",
        "Check the files",
        &[],
        tool_calls,
        0,
        false,
        0.3,
        &make_eval(false, 0.4),
    );
    // Only 2 real calls (read_file OK + bash FAIL), not 3
    assert_eq!(
        extract_tool_call_count(&eval_event),
        2,
        "legacy surgical removal must still be excluded from tool_call_count"
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
        "git_show",
        "git_show",
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
        "git_show",
        "git_show",
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

    let eval_event = build_turn_evaluation_journal_event(
        Some(session_id),
        Some(1),
        "MiniMax-M2.7",
        "Review the PR changes and provide feedback",
        &[
            "skill".into(),
            "read_file".into(),
            "glob".into(),
            "grep".into(),
            "git_show".into(),
            "bash".into(),
            "write_file".into(),
        ],
        &records,
        0,
        false,
        0.2,
        &make_eval(true, 0.85),
    );
    writer.append(&eval_event).unwrap();

    let events = session_journal::read_journal(session_id).unwrap();

    // Journal should persist ALL 21 records for audit
    let tool_calls = events[0].tool_calls.as_ref().unwrap();
    assert_eq!(tool_calls.len(), 21);

    // But evaluation tool_call_count should be 12 (only real)
    let eval = &events[1];
    assert_eq!(
        extract_tool_call_count(eval),
        12,
        "session 3f4389fe turn 1: 21 total records but only 12 real tool calls"
    );

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
    assert!(original_names.contains(&"git_show"));
}

// ─── Unhappy path: all tool calls fail (worst case turn) ────────────────────

#[test]
fn e2e_all_tools_fail_with_surgical_removals_worst_case() {
    let tmp = tempfile::tempdir().unwrap();
    let _guard = JournalDirGuard::new(tmp.path());
    let session_id = "e2e-worst-case";

    let records = vec![
        real_tool("bash", false, 200),      // real failure
        real_tool("write_file", false, 30), // real failure
        surgical_removal("read_file"),      // not a failure
        surgical_removal("glob"),           // not a failure
        skipped_tool("grep"),               // skill-routed, not a failure
    ];

    let eval_event = build_turn_evaluation_journal_event(
        Some(session_id),
        Some(1),
        "test-model",
        "do something",
        &[],
        &records,
        3,    // stalls
        true, // verdict warning
        0.9,  // high budget pressure
        &make_eval(false, 0.1),
    );

    // tool_call_count: only 2 real calls (both failed)
    assert_eq!(
        extract_tool_call_count(&eval_event),
        2,
        "only 2 real tool calls (both failed) — surgical/skipped excluded"
    );

    // Verify the evaluation event still has correct metadata structure
    let meta = eval_event.metadata.as_ref().unwrap();
    assert_eq!(meta["stall_count"], 3);
    assert_eq!(meta["verdict_warning"], true);
    assert!(meta["budget_pressure"].as_f64().unwrap() > 0.8);
    assert_eq!(meta["success"], false);
}

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
    let tool_calls = events[0].tool_calls.as_ref().unwrap();
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

    let eval_event = build_turn_evaluation_journal_event(
        Some(session_id),
        Some(1),
        "gpt-5.4",
        "Complex task",
        &[],
        &records,
        0,
        false,
        0.5,
        &make_eval(true, 0.7),
    );
    writer.append(&eval_event).unwrap();

    let events = session_journal::read_journal(session_id).unwrap();
    let tool_calls = events[0].tool_calls.as_ref().unwrap();
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

    assert_eq!(extract_tool_call_count(&events[1]), 5);
}
