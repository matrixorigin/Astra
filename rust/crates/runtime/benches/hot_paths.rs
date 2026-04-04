//! Criterion benchmarks for performance-critical paths.
//!
//! Run: `cargo bench -p astra-runtime --bench hot_paths`

use criterion::{BenchmarkId, Criterion, black_box, criterion_group, criterion_main};
use serde_json::json;

use astra_runtime::bridge::sse_events::{find_sse_frame_end, parse_sse_json_frame};
use astra_runtime::prompts::{CompactionTier, estimate_str_tokens, estimate_tokens};
use astra_runtime::text_tokenize::{build_tf, tokenize};
use astra_runtime::tool_registry::ConversationState;
use astra_runtime::tool_registry::TOOL_CATALOG;
use astra_runtime::tool_registry::scoring::pre_filter_dynamic;
use astra_runtime::tool_registry::tool_pool::{
    SearchableToolMeta, ToolDenyPredicate, ToolPool, ToolSearchConfig, ToolSource, select_two_phase,
};
use astra_runtime::turn::cloud::compaction::compact_tiered;

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
        b.iter(|| estimate_tokens(black_box(msgs)))
    });
    group.bench_with_input(
        BenchmarkId::new("20_messages", 20),
        &large_msgs,
        |b, msgs| b.iter(|| estimate_tokens(black_box(msgs))),
    );
    group.finish();
}

// ── Tool Selection Pipeline ────────────────────────────────────────

fn bench_pre_filter_dynamic(c: &mut Criterion) {
    let queries = [
        ("github_fetch", "list open PRs in matrixorigin/matrixone"),
        ("git_local", "show me the last 5 commits"),
        ("analytical_cn", "分析一下之前的决策"),
        ("vague_0sig", "我关注matrixorigin"),
        ("memory_store", "记住我喜欢用 Rust"),
        ("code_write", "create a new file called main.rs"),
    ];

    let mut group = c.benchmark_group("pre_filter_dynamic");
    for (label, query) in &queries {
        let state = ConversationState::from_message(query, 3);
        group.bench_with_input(BenchmarkId::new(*label, query.len()), query, |b, q| {
            b.iter(|| pre_filter_dynamic(black_box(&state), black_box(q)))
        });
    }
    group.finish();
}

// ── Two-phase ToolPool selection ─────────────────────────────────────

struct DenyNone;
impl ToolDenyPredicate for DenyNone {
    fn denied(&self, _tool_name: &str) -> bool {
        false
    }
}

#[derive(Clone)]
struct MapStore {
    map: std::collections::HashMap<String, serde_json::Value>,
}

impl astra_runtime::tool_registry::tool_pool::ToolSchemaStore for MapStore {
    fn schema_by_name(&self, name: &str) -> Option<serde_json::Value> {
        self.map.get(name).cloned()
    }
}

fn build_synthetic_pool(n: usize) -> ToolPool<MapStore> {
    // Include built-in catalog entries first (so pinned tools exist).
    let mut index: Vec<SearchableToolMeta> = TOOL_CATALOG
        .iter()
        .map(SearchableToolMeta::from_catalog)
        .collect();

    // Add synthetic tools to simulate huge MCP/plugin pools.
    // Deterministic pseudo-random text without depending on rand crate.
    for i in 0..n {
        index.push(SearchableToolMeta {
            name: format!("mcp__synthetic_tool_{i}"),
            short: format!("Synthetic tool {i} for searching logs, prs, diffs, memory, code"),
            intents: vec!["search", "inspect"],
            estimated_schema_tokens: 40,
            pinned: false,
            source: ToolSource::Mcp,
        });
    }

    let mut map = std::collections::HashMap::new();
    // Provide minimal schemas for all tools in the index.
    for m in &index {
        map.insert(
            m.name.clone(),
            json!({
                "type":"function",
                "function": {
                    "name": m.name,
                    "description": m.short,
                    "parameters": {"type":"object","properties":{}}
                }
            }),
        );
    }

    ToolPool {
        index,
        store: MapStore { map },
    }
}

fn bench_two_phase_tool_pool(c: &mut Criterion) {
    let sizes = [0usize, 1_000, 10_000];
    let terms = [
        "matrixorigin".to_string(),
        "latest".to_string(),
        "pr".to_string(),
    ];
    let cfg = ToolSearchConfig {
        max_candidates: 24,
        budget_tokens: 1200,
        max_prior_discovered: 0,
    };

    let mut group = c.benchmark_group("tool_pool_two_phase");
    for n in sizes {
        let pool = build_synthetic_pool(n);
        group.bench_with_input(BenchmarkId::new("index_size", n), &pool, |b, p| {
            b.iter(|| {
                let out =
                    select_two_phase(black_box(p), black_box(&DenyNone), black_box(&terms), cfg);
                black_box(out.len())
            })
        });
    }
    group.finish();
}

// ── Tiered Compaction ──────────────────────────────────────────────

fn build_conversation(n_turns: usize, tool_output_size: usize) -> Vec<serde_json::Value> {
    let mut msgs = vec![json!({"role":"system","content":"You are a helpful assistant."})];
    for i in 0..n_turns {
        msgs.push(json!({"role":"user","content":format!("Question {}", i)}));
        msgs.push(
            json!({"role":"assistant","content":format!("Let me check tool_{}", i),
            "tool_calls":[{"id":format!("t{}", i),"type":"function",
                "function":{"name":"bash","arguments":"{}"}}]}),
        );
        msgs.push(json!({
            "role":"tool",
            "tool_call_id": format!("t{}", i),
            "content": "x".repeat(tool_output_size)
        }));
        msgs.push(json!({"role":"assistant","content":format!("Here's the result for Q{}", i)}));
    }
    msgs
}

fn bench_compact_tiered(c: &mut Criterion) {
    // 10 turns × 2000 char tool outputs = ~20k chars total
    let msgs = build_conversation(10, 2000);
    let budget_chars = 8000;
    let keep_chars = 500;

    let mut group = c.benchmark_group("compact_tiered");
    for tier in [
        CompactionTier::Normal,
        CompactionTier::TrimSchemas,
        CompactionTier::CompactHistory,
        CompactionTier::AggressivePrune,
    ] {
        group.bench_with_input(
            BenchmarkId::new(format!("{:?}", tier), msgs.len()),
            &msgs,
            |b, m| {
                b.iter(|| {
                    compact_tiered(
                        black_box(m),
                        black_box(budget_chars),
                        black_box(keep_chars),
                        tier,
                        4,
                    )
                })
            },
        );
    }
    group.finish();
}

// ── SSE Frame Parsing ──────────────────────────────────────────────

fn bench_find_sse_frame_end(c: &mut Criterion) {
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
    bench_pre_filter_dynamic,
    bench_two_phase_tool_pool,
    bench_compact_tiered,
    bench_find_sse_frame_end,
    bench_parse_sse_json_frame,
    bench_conversation_state,
);
criterion_main!(benches);
