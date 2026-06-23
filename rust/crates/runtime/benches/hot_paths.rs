//! Criterion benchmarks for performance-critical paths.
//!
//! Run: `cargo bench -p astra-runtime --bench hot_paths`

use criterion::{BenchmarkId, Criterion, black_box, criterion_group, criterion_main};
use serde_json::json;

use astra_runtime::bridge::sse_events::{find_sse_frame_end, parse_sse_json_frame};
use astra_runtime::prompts::{estimate_str_tokens, estimate_tokens};
use astra_runtime::text_tokenize::{build_tf, tokenize};
use astra_runtime::tool_registry::ConversationState;

// ── Tool Surface: always-load build (hot path, every turn) ──────

fn bench_build_always_load_surface(c: &mut Criterion) {
    let catalog = astra_tools::schemas::all_tool_schemas();
    let cfg = astra_config::ToolSurfaceConfig::default();

    let mut group = c.benchmark_group("build_always_load_surface");
    group.bench_function("default_13_always_load", |b| {
        b.iter(|| {
            astra_runtime::tool_registry::surface::ToolSurface::build(
                black_box(catalog.clone()),
                black_box(&cfg),
                black_box(&[]),
            )
        })
    });
    group.finish();
}

// ── Tool Surface: default_always_load_names (LazyLock derivation) ──────

fn bench_default_always_load_names(c: &mut Criterion) {
    let mut group = c.benchmark_group("default_always_load_names");
    group.bench_function("cached_access", |b| {
        b.iter(|| astra_runtime::tool_registry::surface::default_always_load_names())
    });
    group.finish();
}

// ── Schema Prune: inject_required_tool_names (O(n+m) HashMap path) ─

fn bench_inject_required_tool_names(c: &mut Criterion) {
    let all_schemas: Vec<serde_json::Value> = astra_tools::schemas::all_tool_schemas();
    let required: &[&str] = &["bash", "skill", "tool_search", "task", "memory"];
    let surface = vec![
        all_schemas
            .iter()
            .find(|s| {
                s.get("function")
                    .and_then(|f| f.get("name"))
                    .and_then(|n| n.as_str())
                    == Some("read_file")
            })
            .cloned()
            .unwrap(),
        all_schemas
            .iter()
            .find(|s| {
                s.get("function")
                    .and_then(|f| f.get("name"))
                    .and_then(|n| n.as_str())
                    == Some("write_file")
            })
            .cloned()
            .unwrap(),
    ];
    use astra_turn_core::tool_registry_report::ToolSurfaceReport;
    let report = ToolSurfaceReport {
        visible_count: surface.len() as u32,
        visible_tools: surface
            .iter()
            .filter_map(|s| {
                s.get("function")
                    .and_then(|f| f.get("name"))
                    .and_then(|n| n.as_str())
                    .map(String::from)
            })
            .collect(),
        budget_used: 0,
        budget_total: 800,
    };

    let mut group = c.benchmark_group("inject_required_tool_names");
    group.bench_function("5_required_into_2_visible", |b| {
        b.iter(|| {
            let mut s = black_box(surface.clone());
            let mut r = black_box(report.clone());
            astra_turn_core::tool_schema_prune::inject_required_tool_names(
                black_box(&mut s),
                black_box(&mut r),
                black_box(required),
                black_box(&all_schemas),
            )
        })
    });
    group.finish();
}

// ── Schema Prune: retain_invoked_tool_schemas (O(n+m) HashMap path) ──

fn bench_retain_invoked_tool_schemas(c: &mut Criterion) {
    let all_schemas: Vec<serde_json::Value> = astra_tools::schemas::all_tool_schemas();
    let tool_results: Vec<serde_json::Value> = vec![
        json!({"name": "web_fetch", "tool_call_id": "c1", "content": "ok"}),
        json!({"name": "github", "tool_call_id": "c2", "content": "{}"}),
        json!({"name": "mo_query", "tool_call_id": "c3", "content": "ok"}),
        json!({"name": "str_replace", "tool_call_id": "c4", "content": "ok"}),
        json!({"name": "bash", "tool_call_id": "c5", "content": "ok"}),
    ];
    let surface = vec![
        all_schemas
            .iter()
            .find(|s| {
                s.get("function")
                    .and_then(|f| f.get("name"))
                    .and_then(|n| n.as_str())
                    == Some("read_file")
            })
            .cloned()
            .unwrap(),
    ];
    use astra_turn_core::tool_registry_report::ToolSurfaceReport;
    let report = ToolSurfaceReport {
        visible_count: surface.len() as u32,
        visible_tools: surface
            .iter()
            .filter_map(|s| {
                s.get("function")
                    .and_then(|f| f.get("name"))
                    .and_then(|n| n.as_str())
                    .map(String::from)
            })
            .collect(),
        budget_used: 0,
        budget_total: 800,
    };

    let mut group = c.benchmark_group("retain_invoked_tool_schemas");
    group.bench_function("5_results_1_visible", |b| {
        b.iter(|| {
            let mut s = black_box(surface.clone());
            let mut r = black_box(report.clone());
            astra_turn_core::tool_schema_prune::retain_invoked_tool_schemas(
                black_box(&mut s),
                black_box(&mut r),
                black_box(&tool_results),
                black_box(&all_schemas),
            )
        })
    });
    group.finish();
}

// ── Token Estimation ───────────────────────────────────────────────

fn bench_estimate_str_tokens(c: &mut Criterion) {
    let ascii = "The quick brown fox jumps over the lazy dog repeatedly for tokens";
    let cjk = "你好世界这是一个很长的中文句子用来测试分词效果和性能";
    let mixed = "Create issue for matrixorigin/matrixone 关于性能优化的讨论";

    let mut group = c.benchmark_group("estimate_str_tokens");
    group.bench_with_input(BenchmarkId::new("ascii_64", ascii.len()), ascii, |b, s| {
        b.iter(|| estimate_str_tokens(black_box(s)))
    });
    group.bench_with_input(BenchmarkId::new("cjk_50", cjk.len()), cjk, |b, s| {
        b.iter(|| estimate_str_tokens(black_box(s)))
    });
    group.bench_with_input(BenchmarkId::new("mixed_56", mixed.len()), mixed, |b, s| {
        b.iter(|| estimate_str_tokens(black_box(s)))
    });

    // Large input (~4KB)
    let large = ascii.repeat(64);
    group.bench_with_input(BenchmarkId::new("ascii_4k", large.len()), &large, |b, s| {
        b.iter(|| estimate_str_tokens(black_box(s)))
    });
    group.finish();
}

fn bench_estimate_tokens_messages(c: &mut Criterion) {
    let small_msgs: Vec<serde_json::Value> = vec![
        json!({"role":"system","content":"You are an assistant."}),
        json!({"role":"user","content":"Hello world"}),
    ];
    let large_msgs: Vec<serde_json::Value> = (0..20)
        .map(|i| {
            json!({
                "role": if i % 2 == 0 { "user" } else { "assistant" },
                "content": format!("Message {} with some content about various topics and analysis 这是中文内容 {}", i, "x".repeat(200))
            })
        })
        .collect();

    let mut group = c.benchmark_group("estimate_tokens");
    group.bench_with_input(BenchmarkId::new("2_messages", 2), &small_msgs, |b, msgs| {
        b.iter(|| estimate_tokens(black_box(msgs), 0, 0))
    });
    group.bench_with_input(
        BenchmarkId::new("20_messages", 20),
        &large_msgs,
        |b, msgs| b.iter(|| estimate_tokens(black_box(msgs), 0, 0)),
    );
    group.finish();
}

// ── Tiered Compaction ──────────────────────────────────────────────

fn bench_sse_frame_end(c: &mut Criterion) {
    // Frame at start
    let early_frame = b"data: {\"type\":\"text\"}\n\ndata: {\"type\":\"done\"}\n\n";
    // Frame at end of 4KB buffer
    let mut late_frame = vec![b'x'; 4000];
    late_frame.extend_from_slice(b"data: {\"type\":\"text\"}\n\n");
    // No frame (miss)
    let no_frame = vec![b'x'; 4000];

    let mut group = c.benchmark_group("find_sse_frame_end");
    group.bench_with_input(
        BenchmarkId::new("early_hit", early_frame.len()),
        &early_frame[..],
        |b, buf| b.iter(|| find_sse_frame_end(black_box(buf))),
    );
    group.bench_with_input(
        BenchmarkId::new("late_hit_4k", late_frame.len()),
        late_frame.as_slice(),
        |b, buf| b.iter(|| find_sse_frame_end(black_box(buf))),
    );
    group.bench_with_input(
        BenchmarkId::new("miss_4k", no_frame.len()),
        no_frame.as_slice(),
        |b, buf| b.iter(|| find_sse_frame_end(black_box(buf))),
    );
    group.finish();
}

fn bench_parse_sse_json_frame(c: &mut Criterion) {
    let small_frame = b"data: {\"type\":\"text_delta\",\"delta\":\"hello\"}\n\n";
    let large_json = format!(
        "data: {{\"type\":\"tool_call\",\"arguments\":\"{}\"}}\n\n",
        "x".repeat(2000)
    );

    let mut group = c.benchmark_group("parse_sse_json_frame");
    group.bench_with_input(
        BenchmarkId::new("small_text_delta", small_frame.len()),
        &small_frame[..],
        |b, buf| b.iter(|| parse_sse_json_frame(black_box(buf))),
    );
    group.bench_with_input(
        BenchmarkId::new("large_tool_call", large_json.len()),
        large_json.as_bytes(),
        |b, buf| b.iter(|| parse_sse_json_frame(black_box(buf))),
    );
    group.finish();
}

// ── ConversationState Extraction ───────────────────────────────────

fn bench_conversation_state(c: &mut Criterion) {
    let queries = [
        ("en_simple", "list all PRs"),
        ("cn_mixed", "帮我创建一个issue关于matrixone的性能问题"),
        ("long_context", &"analyze the recent changes in the repository and provide a summary of what was modified ".repeat(5)),
    ];

    let mut group = c.benchmark_group("conversation_state");
    for (label, query) in &queries {
        group.bench_with_input(BenchmarkId::new(*label, query.len()), query, |b, q| {
            b.iter(|| ConversationState::from_message(black_box(q), black_box(5)))
        });
    }
    group.finish();
}

// ── Tokenizer ──────────────────────────────────────────────────────

fn bench_tokenize(c: &mut Criterion) {
    let inputs = [
        ("ascii_short", "list open PRs"),
        (
            "ascii_long",
            "analyze the recent changes in the repository and provide a summary of what was modified in each commit",
        ),
        ("cjk_pure", "帮我分析这个仓库的结构和性能问题以及优化方案"),
        (
            "mixed_typical",
            "create issue for matrixorigin/matrixone 关于性能优化的讨论",
        ),
        (
            "mixed_code",
            "fix the bug in src/tool_registry/scoring.rs where tokenize fails on CJK bigrams",
        ),
    ];

    let mut group = c.benchmark_group("tokenize");
    for (label, input) in &inputs {
        group.bench_with_input(BenchmarkId::new(*label, input.len()), input, |b, s| {
            b.iter(|| tokenize(black_box(s)))
        });
    }
    group.finish();
}

fn bench_build_tf(c: &mut Criterion) {
    let short_tokens = tokenize("list open PRs");
    let long_tokens = tokenize("帮我分析这个仓库的结构和性能问题以及优化方案还有改进建议");
    let mixed_tokens =
        tokenize("create issue for matrixorigin/matrixone 关于性能优化的讨论 analyze code changes");

    let mut group = c.benchmark_group("build_tf");
    group.bench_with_input(
        BenchmarkId::new("3_tokens", short_tokens.len()),
        &short_tokens,
        |b, t| b.iter(|| build_tf(black_box(t))),
    );
    group.bench_with_input(
        BenchmarkId::new("30_tokens_cjk", long_tokens.len()),
        &long_tokens,
        |b, t| b.iter(|| build_tf(black_box(t))),
    );
    group.bench_with_input(
        BenchmarkId::new("15_tokens_mixed", mixed_tokens.len()),
        &mixed_tokens,
        |b, t| b.iter(|| build_tf(black_box(t))),
    );
    group.finish();
}

// ── Groups ─────────────────────────────────────────────────────────

criterion_group!(
    benches,
    bench_estimate_str_tokens,
    bench_estimate_tokens_messages,
    bench_tokenize,
    bench_build_tf,
    bench_sse_frame_end,
    bench_parse_sse_json_frame,
    bench_conversation_state,
    bench_build_always_load_surface,
    bench_default_always_load_names,
    bench_inject_required_tool_names,
    bench_retain_invoked_tool_schemas,
);
criterion_main!(benches);
