//! Context pipeline integration tests (Phase 3).
//!
//! These tests exercise the compression / dedup / trace modules at the
//! *pipeline* seam — i.e. with inputs shaped like real tool_result payloads
//! that the runtime actually produces, verifying the contracts documented in
//! `plan-context-cache.md`:
//!
//!   * `cp-compression-roundtrip` — oversized JSON / listing / raw blobs
//!     compress to within budget, preserve structure, carry the marker
//!   * `cp-oversized-single-message` — 500K raw content flows through
//!     sanitize + compress + truncate without exceeding `MAX_TOOL_RESULT_CHARS`
//!   * `cp-dedup-cross-turn` — repeated calls hit cache; write invalidates reads
//!   * `cp-context-assembly-trace-serialize` — full trace struct round-trips
//!     through JSON preserving all documented fields

use astra_turn_core::context_assembly_trace::{
    ContextAssemblyTrace, ContextAssemblyTraceBuilder, SystemPromptBreakdown, TokenBudgetTrace,
    TraceAggregation,
};
use astra_turn_core::tool_result_compression::{
    COMPRESSION_MARKER, DEFAULT_COMPRESSION_BUDGET_CHARS, compress_result_for_context,
    compress_with_default_budget,
};
use astra_turn_core::tool_result_dedup::{CallSignature, ResultCache, new_shared_cache};
use astra_turn_core::tool_result_sanitize::{MAX_TOOL_RESULT_CHARS, tool_result_content_for_model};
use serde_json::{Value, json};

// ─── cp-compression-roundtrip ─────────────────────────────────────────────

#[test]
fn json_array_compression_preserves_head_tail_and_elides_middle() {
    let arr: Vec<Value> = (0..500)
        .map(|i| json!({"i": i, "pad": "x".repeat(200)}))
        .collect();
    let content = serde_json::to_string(&arr).unwrap();
    assert!(content.len() > DEFAULT_COMPRESSION_BUDGET_CHARS);

    let out = compress_with_default_budget("grep", &content);
    assert!(out.contains(COMPRESSION_MARKER), "marker missing in output");
    // head item 0 and tail item 499 should both survive
    assert!(out.contains("\"i\":0"), "head item missing");
    assert!(out.contains("\"i\":499"), "tail item missing");
    // middle items should not all survive
    assert!(
        !out.contains("\"i\":250"),
        "middle item leaked: compression failed"
    );
}

#[test]
fn line_listing_compression_keeps_head_and_tail() {
    let mut content = String::new();
    for i in 0..2000 {
        content.push_str(&format!("line-{i:04}\n"));
    }
    assert!(content.len() > DEFAULT_COMPRESSION_BUDGET_CHARS);

    let out = compress_with_default_budget("grep", &content);
    assert!(out.contains(COMPRESSION_MARKER));
    assert!(out.contains("line-0000"), "first line should be preserved");
    assert!(out.contains("line-1999"), "last line should be preserved");
    assert!(!out.contains("line-1000"), "middle line should be elided");
}

#[test]
fn noop_when_within_budget() {
    let content = "already small";
    let out = compress_result_for_context("any", content, DEFAULT_COMPRESSION_BUDGET_CHARS);
    assert_eq!(out, content);
    assert!(!out.contains(COMPRESSION_MARKER));
}

#[test]
fn fallback_head_tail_for_opaque_blob() {
    // Single-line base64-like blob: not JSON, not line listing.
    let content: String = "Z".repeat(60_000);
    let out = compress_with_default_budget("read_file", &content);
    assert!(
        out.len() < content.len(),
        "fallback compression did not shrink blob"
    );
    assert!(out.contains(COMPRESSION_MARKER), "fallback missing marker");
}

// ─── cp-oversized-single-message ──────────────────────────────────────────

#[test]
fn sanitize_then_compress_for_500k_payload_fits_max() {
    // Simulate a tool result 10x the hard cap.
    let raw: String = "A".repeat(500_000);
    let sanitized = tool_result_content_for_model("read_file", &raw);
    assert!(
        sanitized.len() <= MAX_TOOL_RESULT_CHARS,
        "sanitize did not enforce MAX_TOOL_RESULT_CHARS: len={}",
        sanitized.len()
    );

    // Semantic compressor on the already-sanitized payload is still useful
    // (it can recover structure when the truncator left head + tail slices).
    let compressed = compress_with_default_budget("read_file", &sanitized);
    assert!(compressed.len() <= sanitized.len());
}

// ─── cp-dedup-cross-turn ──────────────────────────────────────────────────

#[test]
fn repeat_call_same_signature_hits_cache_across_turns() {
    let mut cache = ResultCache::new(16, None);
    let args = json!({"path": "/workspace/README.md"});
    let sig = CallSignature::from_args("read_file", &args);

    // Turn 1: miss → record.
    assert!(cache.lookup(&sig).is_none());
    cache.record(sig.clone(), "file contents A".into());

    // Turn 2: different unrelated call — still a miss for read_file(/other).
    let other = CallSignature::from_args("read_file", &json!({"path": "/workspace/other"}));
    assert!(cache.lookup(&other).is_none());

    // Turn 3: repeat same signature — must hit.
    let hit = cache.lookup(&sig).expect("expected cache hit on repeat");
    assert_eq!(hit, "file contents A");
}

#[test]
fn canonical_arg_form_means_key_order_does_not_break_cache_key() {
    let a = CallSignature::from_args("read_file", &json!({"path": "/a", "max_bytes": 1000}));
    let b = CallSignature::from_args("read_file", &json!({"max_bytes": 1000, "path": "/a"}));
    assert_eq!(
        a.input_hash, b.input_hash,
        "canonicalised args must produce identical hash regardless of key order"
    );
}

#[test]
fn write_tool_invalidates_matching_read_entries() {
    let mut cache = ResultCache::new(8, None);
    let read_sig = CallSignature::from_args("read_file", &json!({"path": "/a"}));
    cache.record(read_sig.clone(), "contents".into());
    assert!(cache.lookup(&read_sig).is_some());

    // Caller-side policy: a write tool invalidates prior reads.
    cache.invalidate_tool("read_file");
    assert!(
        cache.lookup(&read_sig).is_none(),
        "read entry should be gone after invalidate_tool(read_file)"
    );
}

#[tokio::test]
async fn shared_cache_lookup_or_compute_reports_hit_on_second_call() {
    let cache = new_shared_cache(16, None);
    let sig = CallSignature::from_args("grep", &json!({"q": "TODO"}));

    let calls = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));

    let c1 = calls.clone();
    let (r1, hit1) =
        astra_turn_core::tool_result_dedup::lookup_or_compute(&cache, &sig, move || {
            let c1 = c1.clone();
            async move {
                c1.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                "grep result".to_string()
            }
        })
        .await;
    assert!(!hit1);
    assert_eq!(r1, "grep result");

    let c2 = calls.clone();
    let (r2, hit2) =
        astra_turn_core::tool_result_dedup::lookup_or_compute(&cache, &sig, move || {
            let c2 = c2.clone();
            async move {
                c2.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                "SHOULD NOT RUN".to_string()
            }
        })
        .await;
    assert!(hit2, "second lookup_or_compute must report a cache hit");
    assert_eq!(r2, "grep result");
    assert_eq!(
        calls.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "closure should only run on the miss"
    );
}

// ─── cp-context-assembly-trace-serialize ──────────────────────────────────

#[test]
fn trace_builder_round_trips_through_json_preserving_fields() {
    let trace = ContextAssemblyTraceBuilder::new("turn-abc", "sess-xyz")
        .with_system_prompt(SystemPromptBreakdown {
            total_tokens: 1234,
            ..Default::default()
        })
        .with_token_budget(TokenBudgetTrace {
            history_tokens: 4096,
            memory_tokens: 256,
            tool_schema_tokens: 512,
            compression_triggered: true,
            ..Default::default()
        })
        .build();

    let json_str = serde_json::to_string(&trace).expect("trace must serialise");
    let back: ContextAssemblyTrace =
        serde_json::from_str(&json_str).expect("trace must round-trip");

    assert_eq!(back.turn_id, "turn-abc");
    assert_eq!(back.session_id, "sess-xyz");
    assert_eq!(back.system_prompt.total_tokens, 1234);
    assert_eq!(back.token_budget.history_tokens, 4096);
    assert_eq!(back.token_budget.memory_tokens, 256);
    assert_eq!(back.token_budget.tool_schema_tokens, 512);
    assert!(back.token_budget.compression_triggered);
}

#[test]
fn trace_aggregation_computes_averages_and_compression_rate() {
    let mut traces = Vec::new();
    for i in 0..4 {
        let t = ContextAssemblyTraceBuilder::new(format!("t{i}"), "s")
            .with_system_prompt(SystemPromptBreakdown {
                total_tokens: 1000,
                ..Default::default()
            })
            .with_token_budget(TokenBudgetTrace {
                history_tokens: 500 + i as u32 * 100,
                // Odd turns triggered compression.
                compression_triggered: i % 2 == 1,
                ..Default::default()
            })
            .build();
        traces.push(t);
    }

    let agg = TraceAggregation::from_traces(&traces);
    assert_eq!(agg.turn_count, 4);
    assert!((agg.avg_system_prompt_tokens - 1000.0).abs() < f64::EPSILON);
    // (500 + 600 + 700 + 800) / 4 = 650
    assert!((agg.avg_history_tokens - 650.0).abs() < f64::EPSILON);
    // 2 of 4 triggered → 0.5
    assert!((agg.compression_trigger_rate - 0.5).abs() < f64::EPSILON);
}

#[test]
fn empty_trace_aggregation_is_default() {
    let agg = TraceAggregation::from_traces(&[]);
    assert_eq!(agg.turn_count, 0);
    assert_eq!(agg.avg_history_tokens, 0.0);
    assert_eq!(agg.compression_trigger_rate, 0.0);
}
