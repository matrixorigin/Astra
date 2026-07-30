//! Phase-0 microbaselines for local O(history) work at model-window scales.
//!
//! Run:
//! `cargo bench -p astra-runtime --bench context_history`
//!
//! Fixtures are sized by Astra's estimator; this is not an estimator-accuracy
//! benchmark. It intentionally measures only work Astra can remove locally:
//! deep clone, compact JSON serialization, prompt-delta hashing/planning, and
//! compaction. Provider upload remains an unavoidable separate wire cost.

use std::time::Duration;

use astra_runtime::turn::{CompactionEngine, TokenBudget};
use criterion::{BenchmarkId, Criterion, Throughput, black_box, criterion_group, criterion_main};
use serde_json::{Value, json};

const TARGET_PRESSURE_PERCENT: usize = 90;
const STRUCTURAL_TURNS: usize = 50;
const WINDOWS: [(&str, usize); 3] = [("128k", 131_072), ("200k", 204_800), ("1m", 1_000_000)];
const ASCII_PAYLOAD_PATTERN: &[u8] =
    b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789_-,:;. ";
const CJK_PAYLOAD_PATTERN: &[char] = &[
    '上', '下', '文', '窗', '口', '工', '具', '结', '果', '验', '证', '数', '据', '流', '程', '边',
    '界',
];

struct ContextFixture {
    raw_context_tokens: usize,
    usable_input_tokens: usize,
    estimated_tokens: usize,
    serialized_bytes: u64,
    messages: Vec<Value>,
}

fn deterministic_ascii(bytes: usize, seed: usize) -> String {
    (0..bytes)
        .map(|index| {
            ASCII_PAYLOAD_PATTERN[(index.saturating_add(seed)) % ASCII_PAYLOAD_PATTERN.len()]
                as char
        })
        .collect()
}

fn deterministic_cjk(chars: usize, seed: usize) -> String {
    (0..chars)
        .map(|index| CJK_PAYLOAD_PATTERN[(index.saturating_add(seed)) % CJK_PAYLOAD_PATTERN.len()])
        .collect()
}

fn structural_turn(turn: usize, payload_tokens: usize) -> [Value; 4] {
    // Exercise three stable payload classes instead of tuning the fixture to
    // one repeated string: regular ASCII, JSON-shaped tool output, and CJK.
    let user_tokens = payload_tokens.saturating_mul(35) / 100;
    let tool_tokens = payload_tokens.saturating_mul(35) / 100;
    let assistant_tokens = payload_tokens
        .saturating_sub(user_tokens)
        .saturating_sub(tool_tokens);
    let user_payload = deterministic_ascii(user_tokens.saturating_mul(4), turn);
    let tool_payload = deterministic_ascii(tool_tokens.saturating_mul(2), turn + 17);
    let assistant_payload = deterministic_cjk(assistant_tokens.saturating_mul(2) / 3, turn + 31);
    [
        json!({
            "role": "user",
            "content": format!("turn={turn}\n{user_payload}"),
        }),
        json!({
            "role": "assistant",
            "content": "",
            "tool_calls": [{
                "id": format!("call-{turn}"),
                "type": "function",
                "function": {
                    "name": "fixture_tool",
                    "arguments": format!("{{\"turn\":{turn}}}"),
                },
            }],
        }),
        json!({
            "role": "tool",
            "tool_call_id": format!("call-{turn}"),
            "content": format!("{{\"turn\":{turn},\"payload\":\"{tool_payload}\"}}"),
        }),
        json!({
            "role": "assistant",
            "content": format!("reply={turn}\n{assistant_payload}"),
        }),
    ]
}

fn context_fixture(raw_context_tokens: usize) -> ContextFixture {
    let usable_input_tokens = (raw_context_tokens as f64
        * astra_core::runtime_limits::MODEL_CONTEXT_INPUT_BUDGET_RATIO)
        as usize;
    let target_tokens = usable_input_tokens.saturating_mul(TARGET_PRESSURE_PERCENT) / 100;
    let base = (0..STRUCTURAL_TURNS)
        .flat_map(|turn| structural_turn(turn, 0))
        .collect::<Vec<_>>();
    let base_tokens = astra_runtime::prompts::estimate_tokens(&base, 0, 1);
    assert!(
        target_tokens > base_tokens,
        "fixture target must exceed structural overhead"
    );

    let payload_tokens = target_tokens.saturating_sub(base_tokens);
    let per_turn_tokens = payload_tokens / STRUCTURAL_TURNS;
    let remainder = payload_tokens % STRUCTURAL_TURNS;
    let mut messages = (0..STRUCTURAL_TURNS)
        .flat_map(|turn| structural_turn(turn, per_turn_tokens + usize::from(turn < remainder)))
        .collect::<Vec<_>>();
    let mut estimated_tokens = astra_runtime::prompts::estimate_tokens(&messages, 0, 1);

    // Close the small integer-rounding gap without changing the structural
    // topology. This is numeric fixture calibration, never product routing.
    if estimated_tokens < target_tokens {
        let missing_chars = target_tokens
            .saturating_sub(estimated_tokens)
            .saturating_mul(4);
        let content = messages
            .last()
            .and_then(|message| message.get("content"))
            .and_then(Value::as_str)
            .expect("last fixture message has string content")
            .to_string();
        let mut adjusted = String::with_capacity(content.len().saturating_add(missing_chars));
        adjusted.push_str(&content);
        adjusted.push_str(&"z".repeat(missing_chars));
        messages.last_mut().expect("fixture has messages")["content"] = Value::String(adjusted);
        estimated_tokens = astra_runtime::prompts::estimate_tokens(&messages, 0, 1);
    }

    let error = estimated_tokens.abs_diff(target_tokens);
    assert!(
        error <= target_tokens / 1_000 + 1,
        "fixture estimate must be within 0.1% of target: target={target_tokens} actual={estimated_tokens}"
    );
    let serialized_bytes = astra_core::history_work::serialized_bytes(&messages)
        .expect("fixture serialization")
        .max(1);
    ContextFixture {
        raw_context_tokens,
        usable_input_tokens,
        estimated_tokens,
        serialized_bytes,
        messages,
    }
}

fn fixture_id(label: &str, fixture: &ContextFixture) -> BenchmarkId {
    BenchmarkId::new(
        format!(
            "{label}_raw{}_usable{}",
            fixture.raw_context_tokens, fixture.usable_input_tokens
        ),
        fixture.estimated_tokens,
    )
}

fn bench_history_clone(c: &mut Criterion) {
    let fixtures = WINDOWS
        .into_iter()
        .map(|(label, window)| (label, context_fixture(window)))
        .collect::<Vec<_>>();
    let mut group = c.benchmark_group("context_history_clone");
    group.measurement_time(Duration::from_secs(2));
    group.sample_size(10);
    for (label, fixture) in &fixtures {
        group.throughput(Throughput::Bytes(fixture.serialized_bytes));
        group.bench_with_input(fixture_id(label, fixture), fixture, |b, fixture| {
            b.iter(|| black_box(fixture.messages.clone()))
        });
    }
    group.finish();
}

fn bench_history_serialization(c: &mut Criterion) {
    let fixtures = WINDOWS
        .into_iter()
        .map(|(label, window)| (label, context_fixture(window)))
        .collect::<Vec<_>>();
    let mut group = c.benchmark_group("context_history_serialize");
    group.measurement_time(Duration::from_secs(2));
    group.sample_size(10);
    for (label, fixture) in &fixtures {
        group.throughput(Throughput::Bytes(fixture.serialized_bytes));
        group.bench_with_input(fixture_id(label, fixture), fixture, |b, fixture| {
            b.iter(|| serde_json::to_vec(black_box(&fixture.messages)).expect("serialize"))
        });
    }
    group.finish();
}

fn bench_prompt_delta_plan(c: &mut Criterion) {
    let fixtures = WINDOWS
        .into_iter()
        .map(|(label, window)| (label, context_fixture(window)))
        .collect::<Vec<_>>();
    let mut group = c.benchmark_group("context_history_prompt_delta_plan");
    group.measurement_time(Duration::from_secs(2));
    group.sample_size(10);
    for (label, fixture) in &fixtures {
        group.throughput(Throughput::Bytes(fixture.serialized_bytes));
        group.bench_with_input(fixture_id(label, fixture), fixture, |b, fixture| {
            b.iter(|| {
                astra_services::plan_prompt_request(astra_services::PromptRequestPlanInput {
                    user_id: "phase0-baseline-user",
                    session_id: "phase0-baseline-session",
                    turn: 50,
                    round: 3,
                    attempt: 0,
                    source: "criterion_context_history",
                    messages: black_box(&fixture.messages),
                    tools: &[],
                    max_output_tokens: Some(4_096),
                })
                .expect("plan prompt request")
            })
        });
    }
    group.finish();
}

fn bench_high_pressure_compaction(c: &mut Criterion) {
    let fixtures = WINDOWS
        .into_iter()
        .map(|(label, window)| (label, context_fixture(window)))
        .collect::<Vec<_>>();
    let mut group = c.benchmark_group("context_history_high_pressure_compaction");
    group.measurement_time(Duration::from_secs(2));
    group.sample_size(10);
    for (label, fixture) in &fixtures {
        group.throughput(Throughput::Bytes(fixture.serialized_bytes));
        group.bench_with_input(fixture_id(label, fixture), fixture, |b, fixture| {
            let engine = CompactionEngine::default_pipeline_for(fixture.raw_context_tokens as u64);
            let budget = TokenBudget {
                max_prompt_tokens: fixture.usable_input_tokens.saturating_mul(85) as u64 / 100,
                last_measured_tokens: fixture.estimated_tokens as u64,
                current_round_index: Some(3),
                now_secs: 1_800_000_000,
            };
            b.iter_batched(
                || fixture.messages.clone(),
                |mut messages| {
                    black_box(engine.compress_if_needed(&mut messages, &budget));
                    black_box(messages)
                },
                criterion::BatchSize::LargeInput,
            )
        });
    }
    group.finish();
}

criterion_group!(
    context_history_benches,
    bench_history_clone,
    bench_history_serialization,
    bench_prompt_delta_plan,
    bench_high_pressure_compaction,
);
criterion_main!(context_history_benches);
