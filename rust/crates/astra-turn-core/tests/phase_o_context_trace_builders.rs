//! Phase O — context-assembly trace builders coverage.
//!
//! `build_history_trace_from_compression`, `build_tool_trace_from_selection`,
//! and `build_memory_trace_from_retrieval` are public but only the tool
//! variant has direct coverage in-file. These integration tests pin the
//! behaviour of all three builders so downstream telemetry consumers
//! (dashboards, journal playback) see a stable schema.

use astra_turn_core::context_assembly_trace::{
    CompressionMethod, MemorySource, build_history_trace_from_compression,
    build_memory_trace_from_retrieval, build_tool_trace_from_selection,
};

// ── build_history_trace_from_compression ────────────────────────────────────

#[test]
fn phase_o_history_trace_records_each_layer_that_freed_tokens() {
    let layers = vec![
        (
            "ToolResultTruncation".to_string(),
            CompressionMethod::ToolResultTruncation,
            500_u32,
        ),
        (
            "SummarizeOldTurns".to_string(),
            CompressionMethod::LlmSummarization,
            0_u32, // freed nothing — must be skipped
        ),
        (
            "DropEarliest".to_string(),
            CompressionMethod::TieredCompaction,
            1200_u32,
        ),
    ];
    let trace = build_history_trace_from_compression(40, 20, 10_000, 8_300, &layers);

    assert_eq!(trace.turns_compressed.len(), 2, "only layers with >0 freed");
    let names: Vec<_> = trace
        .turns_compressed
        .iter()
        .map(|t| t.role.clone())
        .collect();
    assert!(names.contains(&"ToolResultTruncation".to_string()));
    assert!(names.contains(&"DropEarliest".to_string()));
    // ratio = 8300/10000 = 0.83
    assert!(
        (trace.compression_ratio - 0.83).abs() < 0.001,
        "got {}",
        trace.compression_ratio
    );
    assert_eq!(trace.tokens_before, 10_000);
    assert_eq!(trace.tokens_after, 8_300);
    assert_eq!(trace.total_turns_available, 40);
}

#[test]
fn phase_o_history_trace_zero_initial_tokens_ratio_is_one() {
    let layers: Vec<(String, CompressionMethod, u32)> = vec![];
    let trace = build_history_trace_from_compression(0, 0, 0, 0, &layers);
    assert!((trace.compression_ratio - 1.0).abs() < 1e-9);
    assert!(trace.turns_compressed.is_empty());
}

// ── build_tool_trace_from_selection ─────────────────────────────────────────

#[test]
fn phase_o_tool_trace_maps_costs_by_name() {
    let selected = vec!["bash".to_string(), "read_file".to_string()];
    let costs = vec![("bash".to_string(), 120_u32), ("read_file".to_string(), 80_u32)];
    let trace = build_tool_trace_from_selection(10, &selected, "tfidf", 0.75, &costs, 42);

    assert_eq!(trace.tools_available, 10);
    assert_eq!(trace.tools_selected.len(), 2);
    assert_eq!(trace.selection_strategy, "tfidf");
    assert!((trace.selection_confidence - 0.75).abs() < 0.001);
    assert_eq!(trace.selection_latency_ms, 42);

    let bash = trace
        .tools_selected
        .iter()
        .find(|t| t.tool_name == "bash")
        .unwrap();
    assert_eq!(bash.tokens, 120);
    let read = trace
        .tools_selected
        .iter()
        .find(|t| t.tool_name == "read_file")
        .unwrap();
    assert_eq!(read.tokens, 80);
}

#[test]
fn phase_o_tool_trace_missing_cost_defaults_to_zero() {
    let selected = vec!["bash".to_string()];
    let trace = build_tool_trace_from_selection(5, &selected, "keyword", 0.9, &[], 1);
    assert_eq!(trace.tools_selected[0].tokens, 0);
}

#[test]
fn phase_o_tool_trace_empty_selection_produces_empty_vec() {
    let trace = build_tool_trace_from_selection(5, &[], "none", 0.0, &[], 0);
    assert!(trace.tools_selected.is_empty());
    assert_eq!(trace.tools_available, 5);
}

// ── build_memory_trace_from_retrieval ───────────────────────────────────────

#[test]
fn phase_o_memory_trace_preview_truncated_at_100_chars() {
    let long = "a".repeat(250);
    let results = vec![(long.clone(), 0.9_f64)];
    let trace = build_memory_trace_from_retrieval("what happened last time?", 5, &results, 12);

    assert_eq!(trace.query, "what happened last time?");
    assert_eq!(trace.candidates_considered, 5);
    assert_eq!(trace.retrieval_latency_ms, 12);
    assert_eq!(trace.memories_selected.len(), 1);
    let m = &trace.memories_selected[0];
    // Preview must include an ellipsis when original > 100 chars.
    assert!(m.content_preview.ends_with("..."));
    // And be ≤ 103 chars (100 + "...").
    assert!(m.content_preview.len() <= 103);
    assert!((m.relevance_score - 0.9).abs() < 1e-9);
    assert_eq!(m.tokens, (250 / 4) as u32);
    assert!(matches!(m.source, MemorySource::Session));
}

#[test]
fn phase_o_memory_trace_short_content_preview_is_unchanged() {
    let short = "a short memory";
    let results = vec![(short.to_string(), 0.5_f64)];
    let trace = build_memory_trace_from_retrieval("q", 1, &results, 3);
    assert_eq!(trace.memories_selected[0].content_preview, short);
    assert!(!trace.memories_selected[0].content_preview.ends_with("..."));
}

#[test]
fn phase_o_memory_trace_utf8_boundary_is_respected() {
    // A 100-byte boundary that would split a multi-byte codepoint must be
    // respected via `floor_char_boundary`. Use a string full of 3-byte chars.
    let weird = "中".repeat(80); // 80 * 3 = 240 bytes, all codepoints 3 bytes.
    let results = vec![(weird.clone(), 0.5_f64)];
    let trace = build_memory_trace_from_retrieval("q", 1, &results, 0);
    // Preview must still be valid UTF-8 and end with "..." without panic.
    let p = &trace.memories_selected[0].content_preview;
    assert!(p.ends_with("..."));
    // Must not have panicked and result is a valid string (guaranteed by Rust).
    assert!(p.is_char_boundary(p.len()));
}

#[test]
fn phase_o_memory_trace_total_tokens_is_sum_of_entries() {
    let results = vec![
        ("a".repeat(40), 0.9_f64),
        ("b".repeat(80), 0.8_f64),
        ("c".repeat(120), 0.7_f64),
    ];
    let trace = build_memory_trace_from_retrieval("q", 3, &results, 0);
    // Each entry's tokens field is content.len()/4.
    let expected: u32 = 40 / 4 + 80 / 4 + 120 / 4;
    assert_eq!(trace.total_tokens, expected);
}
