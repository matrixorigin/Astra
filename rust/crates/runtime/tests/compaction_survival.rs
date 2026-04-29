//! Compaction survival tests — verifying tool results remain available through
//! realistic multi-round agentic workflows.
//!
//! Context compaction is now a single unified pass (compact_tool_results_adaptive)
//! running before each LLM call. fold_old_read_only_results is a no-op and
//! run_micro_compact has been removed.
//!
//! These tests encode the invariant: **within a single user turn, tool results
//! must survive long enough for the model to act on them.**

use serde_json::{Value, json};

// ─── Helpers ────────────────────────────────────────────────────────────

fn assistant_tool_calls(calls: &[(&str, &str)]) -> Value {
    let tc: Vec<Value> = calls
        .iter()
        .map(|(id, name)| {
            json!({
                "id": id,
                "type": "function",
                "function": { "name": name, "arguments": "{}" }
            })
        })
        .collect();
    json!({ "role": "assistant", "content": null, "tool_calls": tc })
}

fn tool_result(call_id: &str, content: &str) -> Value {
    json!({ "role": "tool", "tool_call_id": call_id, "content": content })
}

fn tool_result_with_round(call_id: &str, content: &str, round: u32, tool_name: &str) -> Value {
    json!({
        "role": "tool",
        "tool_call_id": call_id,
        "content": content,
        "_round_index": round,
        "_tool_name": tool_name,
    })
}

fn make_file_content(path: &str, lines: usize) -> String {
    (1..=lines)
        .map(|i| format!("{i}\t    {path} line {i} content here"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn is_content_usable(content: &str) -> bool {
    !content.contains("[folded:")
        && !content.contains("[Cleared")
        && !content.contains("[Previous tool output cleared]")
        && !content.contains("[tool result cleared")
        && !content.contains("(cached")
        && content.len() > 50
}

// ─── Scenario: Review then Fix ──────────────────────────────────────────
//
// Reproduces session d5c35a57: Turn 2 reviews code (7 rounds of reads),
// Turn 3 fixes issues (needs the read content to apply str_replace).
//
// The model reads 3 files in round 0, greps in rounds 1-2, reads again
// in round 3, then needs to str_replace in round 4+. The read content
// from round 0 must survive at least through round 5.

/// After a review turn reads files, the fix turn's first read_file results
/// must not be folded/cleared before the model can act on them.
#[test]
fn read_results_survive_through_edit_round() {
    let file_a = make_file_content("skill_tool.rs", 80);
    let file_b = make_file_content("server_loop_host.rs", 60);
    let file_c = make_file_content("tool_registry_meta.rs", 30);

    // Round 0: model reads 3 files
    let mut messages = vec![
        json!({"role": "user", "content": "fix the issues found in review"}),
        assistant_tool_calls(&[
            ("r0-a", "read_file"),
            ("r0-b", "read_file"),
            ("r0-c", "read_file"),
        ]),
        tool_result_with_round("r0-a", &file_a, 0, "read_file"),
        tool_result_with_round("r0-b", &file_b, 0, "read_file"),
        tool_result_with_round("r0-c", &file_c, 0, "read_file"),
    ];

    // Round 1: model greps for context
    messages.push(assistant_tool_calls(&[("r1-a", "grep"), ("r1-b", "bash")]));
    messages.push(tool_result_with_round(
        "r1-a",
        "skill_tool.rs:1132: intersection logic here",
        1,
        "grep",
    ));
    messages.push(tool_result_with_round(
        "r1-b",
        "request_constraints found in 5 files",
        1,
        "bash",
    ));

    // Round 2: model reads more files
    messages.push(assistant_tool_calls(&[("r2-a", "read_file")]));
    messages.push(tool_result_with_round(
        "r2-a",
        &make_file_content("delegate_interception.rs", 40),
        2,
        "read_file",
    ));

    // Round 3: model greps again
    messages.push(assistant_tool_calls(&[("r3-a", "grep")]));
    messages.push(tool_result_with_round(
        "r3-a",
        "struct RequestConstraints found at line 260",
        3,
        "grep",
    ));

    // Now simulate what happens before round 4 (the edit round):
    // The system runs fold_old_read_only_results with current_round=4.
    astra_runtime::turn::context_compression::fold_old_read_only_results(&mut messages, 4);

    // INVARIANT: The round-0 file reads must still be usable.
    // The model needs the full code content to craft a str_replace.
    let r0a = messages[2]["content"].as_str().unwrap();
    let r0b = messages[3]["content"].as_str().unwrap();
    let r0c = messages[4]["content"].as_str().unwrap();

    assert!(
        is_content_usable(r0a),
        "file_a (round 0) must survive through round 4 for edit. Got: {}",
        &r0a[..r0a.len().min(200)]
    );
    assert!(
        is_content_usable(r0b),
        "file_b (round 0) must survive through round 4 for edit. Got: {}",
        &r0b[..r0b.len().min(200)]
    );
    assert!(
        is_content_usable(r0c),
        "file_c (round 0) must survive through round 4 for edit. Got: {}",
        &r0c[..r0c.len().min(200)]
    );
}

/// Even at round 6 (edit + verify cycle), the read from round 0 must be
/// either fully intact or fully cleared (re-runnable), never partially folded.
#[test]
fn no_partial_content_folding_ever() {
    let file_content = make_file_content("skill_tool.rs", 80);

    let mut messages = vec![
        json!({"role": "user", "content": "fix this"}),
        assistant_tool_calls(&[("r0-a", "read_file")]),
        tool_result_with_round("r0-a", &file_content, 0, "read_file"),
    ];

    // Simulate 8 rounds of subsequent work
    for round in 1..=8u32 {
        messages.push(assistant_tool_calls(&[(&format!("r{round}-x"), "grep")]));
        messages.push(tool_result_with_round(
            &format!("r{round}-x"),
            &format!("grep result round {round}"),
            round,
            "grep",
        ));
    }

    astra_runtime::turn::context_compression::fold_old_read_only_results(&mut messages, 9);

    let content = messages[2]["content"].as_str().unwrap();

    // The content must be EITHER:
    // 1. Fully intact (preferred — model can still use it)
    // 2. Fully cleared with a re-run placeholder (model knows to re-read)
    //
    // It must NEVER be partially folded to 200 chars — that's useless garbage
    // that the model can neither use nor recognize as needing re-read.
    let is_full = content.len() == file_content.len();
    let is_cleared = content.contains("[Cleared")
        || content.contains("[Previous tool output cleared]")
        || content.contains("[tool result cleared");
    assert!(
        is_full || is_cleared,
        "Tool result must be fully intact or fully cleared, not partially folded. \
         Got {} chars (original {}): {}",
        content.len(),
        file_content.len(),
        &content[..content.len().min(300)]
    );
}

// ─── Scenario: Cascading compaction destroys content ────────────────────
//
// The triple-compaction cascade: fold truncates to 200 chars, then
// microcompact sees the small result and may skip it (< MIN_COMPACT_SIZE),
// leaving a useless 200-char stub that's neither full nor cleared.

/// When fold + microcompact run sequentially, results must not end up in
/// an unusable intermediate state.
#[test]
fn fold_then_microcompact_no_useless_stubs() {
    let big_content = make_file_content("main.rs", 100);

    let mut messages = vec![
        assistant_tool_calls(&[
            ("c1", "read_file"),
            ("c2", "read_file"),
            ("c3", "read_file"),
            ("c4", "read_file"),
            ("c5", "read_file"),
            ("c6", "read_file"),
            ("c7", "read_file"),
            ("c8", "read_file"),
        ]),
        tool_result_with_round("c1", &big_content, 0, "read_file"),
        tool_result_with_round("c2", &big_content, 0, "read_file"),
        tool_result_with_round("c3", &big_content, 1, "read_file"),
        tool_result_with_round("c4", &big_content, 1, "read_file"),
        tool_result_with_round("c5", &big_content, 2, "read_file"),
        tool_result_with_round("c6", &big_content, 2, "read_file"),
        tool_result_with_round("c7", &big_content, 3, "read_file"),
        tool_result_with_round("c8", &big_content, 3, "read_file"),
    ];

    // Step 1: fold runs (round 5)
    astra_runtime::turn::context_compression::fold_old_read_only_results(&mut messages, 5);

    // Step 2: microcompact runs at low pressure
    astra_turn_core::microcompact::compact_tool_results_adaptive(
        &mut messages,
        0.3,
        Default::default(),
    );

    // Check every tool result is in a valid state
    for (i, msg) in messages.iter().enumerate() {
        if msg.get("role").and_then(|v| v.as_str()) != Some("tool") {
            continue;
        }
        let content = msg["content"].as_str().unwrap_or("");
        let is_full = content.len() > 300;
        let is_properly_cleared = content.contains("[Cleared")
            || content.contains("[Previous tool output cleared]")
            || content.contains("[tool result cleared")
            || content.len() < 100; // short grep results are fine

        assert!(
            is_full || is_properly_cleared,
            "msg[{i}] is in unusable intermediate state: {} chars, starts with: {}",
            content.len(),
            &content[..content.len().min(200)]
        );
    }
}

// ─── Scenario: Analytics micro_compact runs too eagerly ─────────────────

/// With many tool results, the unified compaction (compact_tool_results_adaptive)
/// at low pressure preserves recent results and only clears the oldest when
/// count exceeds keep_recent (6 by default).
#[test]
fn unified_compaction_at_low_pressure_preserves_recent_reads() {
    let file_a = make_file_content("skill_tool.rs", 80);
    let file_b = make_file_content("server_loop_host.rs", 60);

    let mut messages: Vec<Value> = Vec::new();

    // Round 0: read 3 files (these are the ones model will edit)
    messages.push(assistant_tool_calls(&[
        ("r0-a", "read_file"),
        ("r0-b", "read_file"),
        ("r0-c", "read_file"),
    ]));
    messages.push(tool_result("r0-a", &file_a));
    messages.push(tool_result("r0-b", &file_b));
    messages.push(tool_result(
        "r0-c",
        &make_file_content("tool_registry.rs", 30),
    ));

    // Rounds 1-3: analysis (grep, read, bash)
    for round in 1..=3u32 {
        messages.push(assistant_tool_calls(&[
            (&format!("r{round}-x"), "grep"),
            (&format!("r{round}-y"), "read_file"),
            (&format!("r{round}-z"), "bash"),
        ]));
        messages.push(tool_result(
            &format!("r{round}-x"),
            &format!("grep output round {round}: matches found in 5 files"),
        ));
        messages.push(tool_result(
            &format!("r{round}-y"),
            &make_file_content(&format!("analysis_{round}.rs"), 20),
        ));
        messages.push(tool_result(
            &format!("r{round}-z"),
            &format!("bash output round {round}"),
        ));
    }

    // Run the single unified compaction at low pressure
    astra_turn_core::microcompact::compact_tool_results_adaptive(
        &mut messages,
        0.3,
        Default::default(),
    );

    // r0-a and r0-b are the files the model needs to edit.
    // At low pressure (keep_recent=6), with 6 compactable read_file results
    // total, they should all survive.
    let r0a_content = messages[1]["content"].as_str().unwrap_or("");
    let r0b_content = messages[2]["content"].as_str().unwrap_or("");

    assert!(
        is_content_usable(r0a_content),
        "r0-a must survive unified compaction at low pressure: {}",
        &r0a_content[..r0a_content.len().min(100)]
    );
    assert!(
        is_content_usable(r0b_content),
        "r0-b must survive unified compaction at low pressure: {}",
        &r0b_content[..r0b_content.len().min(100)]
    );
}

// ─── Invariant: mutation evidence never compacts ────────────────────────

/// bash and str_replace results must NEVER be compacted, regardless of
/// how many rounds pass or how high the pressure gets.
#[test]
fn mutation_evidence_survives_all_compaction_stages() {
    let bash_output = "Compiling astra-runtime v0.1.0\n    Finished dev [unoptimized + debuginfo] target(s) in 27.02s\nwarning: unused variable `x`";
    let str_replace_output = "Successfully replaced content in skill_tool.rs (4 lines changed)";
    let file_content = make_file_content("big_file.rs", 100);

    let mut messages = vec![
        // Read in round 0
        assistant_tool_calls(&[("r0-read", "read_file")]),
        tool_result_with_round("r0-read", &file_content, 0, "read_file"),
        // Edit + compile in round 1
        assistant_tool_calls(&[("r1-edit", "str_replace"), ("r1-bash", "bash")]),
        tool_result_with_round("r1-edit", str_replace_output, 1, "str_replace"),
        tool_result_with_round("r1-bash", bash_output, 1, "bash"),
        // More reads in rounds 2-5
    ];

    for round in 2..=5u32 {
        messages.push(assistant_tool_calls(&[(
            &format!("r{round}-read"),
            "read_file",
        )]));
        messages.push(tool_result_with_round(
            &format!("r{round}-read"),
            &make_file_content(&format!("file_{round}.rs"), 50),
            round,
            "read_file",
        ));
    }

    // Run both compaction stages (fold is a no-op, unified adaptive is the real pass)
    astra_runtime::turn::context_compression::fold_old_read_only_results(&mut messages, 7);
    astra_turn_core::microcompact::compact_tool_results_adaptive(
        &mut messages,
        0.85,
        Default::default(),
    );

    // str_replace and bash outputs must be completely intact
    let edit_result = messages
        .iter()
        .find(|m| {
            m.get("tool_call_id")
                .and_then(|v| v.as_str())
                .map_or(false, |id| id == "r1-edit")
        })
        .unwrap();
    let bash_result = messages
        .iter()
        .find(|m| {
            m.get("tool_call_id")
                .and_then(|v| v.as_str())
                .map_or(false, |id| id == "r1-bash")
        })
        .unwrap();

    assert_eq!(
        edit_result["content"].as_str().unwrap(),
        str_replace_output,
        "str_replace output must never be compacted"
    );
    assert_eq!(
        bash_result["content"].as_str().unwrap(),
        bash_output,
        "bash output must never be compacted"
    );
}
