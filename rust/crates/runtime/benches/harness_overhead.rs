//! Criterion benchmarks for harness overhead.
//!
//! Run: `cargo bench -p astra-runtime --bench harness_overhead`
//!
//! Targets from spec §13 (Performance):
//! - Feature on + no kernel (HarnessSlot { kernel: None }): per-hook p50 < 100ns
//! - Feature on + StandardKernel + empty verifiers: per-hook p50 < 5μs
//! - Feature on + TurnGuardVerifierAdapter: per-hook p50 < 10μs

use criterion::{Criterion, black_box, criterion_group, criterion_main};
use std::sync::Arc;

use astra_harness::{
    HarnessKernel, HookPoint, HookVerdict, InMemorySnapshotSink, RuntimeSnapshot, SnapshotSink,
    StandardKernel,
    verifiers::{BudgetVerifier, TurnGuardVerifierAdapter},
};

fn make_snapshot() -> RuntimeSnapshot {
    RuntimeSnapshot {
        session_id: "bench-session".into(),
        turn_number: 5,
        model: Some("claude-sonnet-4-6".into()),
        context_total_tokens: Some(80_000),
        context_budget_tokens: Some(200_000),
        context_message_count: 20,
        context_system_prompt_tokens: Some(2_000),
        context_utilization: Some(0.4),
        turns_used: 5,
        turns_limit: Some(25),
        session_turn: 2,
        tokens_used_session: 120_000,
        tokens_prompt: 70_000,
        tokens_completion: 20_000,
        tokens_cache_read: 25_000,
        tokens_cache_creation: 5_000,
        elapsed_millis: 30_000,
        tool_calls_this_session: 8,
        unique_tools_used: vec!["bash".into(), "read_file".into(), "edit_file".into()],
        last_tool_called: Some("bash".into()),
        consecutive_same_tool: 1,
        delegations_this_turn: 0,
        recursion_depth: 0,
        consecutive_errors: 0,
        captured_at_unix_millis: 1_700_000_000_000,
        session_start_unix_millis: 1_700_000_000_000 - 30_000,
        schema_version: 2,
    }
}

fn make_record(point: HookPoint) -> astra_harness::DecisionRecord {
    astra_harness::DecisionRecord {
        session_id: "bench-session".into(),
        turn: 5,
        point,
        wall_time_unix_millis: 1_700_000_000_000,
        monotonic_millis_since_session: 30_000,
        snapshot: make_snapshot(),
    }
}

fn bench_no_kernel(c: &mut Criterion) {
    let mut group = c.benchmark_group("harness_no_kernel");

    // Simulates the hot path: HarnessSlot { kernel: None }
    // This is what happens when harness feature is on but no kernel is configured.
    // Target: < 100ns
    let kernel: Option<Arc<dyn HarnessKernel>> = None;

    group.bench_function("hook_dispatch_none", |b| {
        b.iter(|| {
            match &kernel {
                Some(k) => k.on_record(black_box(&make_record(HookPoint::PostTurn))),
                None => HookVerdict::Continue,
            }
        });
    });

    group.finish();
}

fn bench_empty_verifiers(c: &mut Criterion) {
    let mut group = c.benchmark_group("harness_empty_verifiers");

    let sink = InMemorySnapshotSink::arc();
    let kernel = Arc::new(StandardKernel::new(
        sink as Arc<dyn SnapshotSink>,
        vec![],
    ));

    // Target: per-hook p50 < 5μs
    for point in [
        HookPoint::SessionStart,
        HookPoint::PostLlmResponse,
        HookPoint::PreToolBatch,
        HookPoint::PostToolBatch,
        HookPoint::PostTurn,
    ] {
        group.bench_function(format!("{point:?}"), |b| {
            let record = make_record(point);
            b.iter(|| kernel.on_record(black_box(&record)));
        });
    }

    group.finish();
}

fn bench_budget_verifier(c: &mut Criterion) {
    let mut group = c.benchmark_group("harness_budget_verifier");

    let sink = InMemorySnapshotSink::arc();
    let kernel = Arc::new(StandardKernel::new(
        sink as Arc<dyn SnapshotSink>,
        vec![Box::new(BudgetVerifier {
            max_turns: Some(25),
            max_tokens: Some(500_000),
            max_duration_millis: Some(300_000),
        })],
    ));

    // Target: per-hook p50 < 10μs
    group.bench_function("PostTurn_within_budget", |b| {
        let record = make_record(HookPoint::PostTurn);
        b.iter(|| kernel.on_record(black_box(&record)));
    });

    group.finish();
}

fn bench_turn_guard_adapter(c: &mut Criterion) {
    let mut group = c.benchmark_group("harness_turn_guard_adapter");

    let sink = InMemorySnapshotSink::arc();
    let kernel = Arc::new(StandardKernel::new(
        sink as Arc<dyn SnapshotSink>,
        vec![Box::new(TurnGuardVerifierAdapter::default())],
    ));

    // Target: per-hook p50 < 10μs
    group.bench_function("PostTurn_no_stall", |b| {
        let record = make_record(HookPoint::PostTurn);
        b.iter(|| kernel.on_record(black_box(&record)));
    });

    group.finish();
}

fn bench_combined_verifiers(c: &mut Criterion) {
    let mut group = c.benchmark_group("harness_combined_verifiers");

    let sink = InMemorySnapshotSink::arc();
    let kernel = Arc::new(StandardKernel::new(
        sink as Arc<dyn SnapshotSink>,
        vec![
            Box::new(BudgetVerifier {
                max_turns: Some(25),
                max_tokens: Some(500_000),
                max_duration_millis: Some(300_000),
            }),
            Box::new(TurnGuardVerifierAdapter::default()),
        ],
    ));

    // Target: per-hook p50 < 10μs with both verifiers
    group.bench_function("PostTurn_both_verifiers", |b| {
        let record = make_record(HookPoint::PostTurn);
        b.iter(|| kernel.on_record(black_box(&record)));
    });

    group.finish();
}

fn bench_snapshot_capture(c: &mut Criterion) {
    let mut group = c.benchmark_group("harness_snapshot");

    group.bench_function("clone_snapshot", |b| {
        let snap = make_snapshot();
        b.iter(|| black_box(snap.clone()));
    });

    group.bench_function("serialize_snapshot_json", |b| {
        let snap = make_snapshot();
        b.iter(|| serde_json::to_string(black_box(&snap)).unwrap());
    });

    group.finish();
}

criterion_group!(
    benches,
    bench_no_kernel,
    bench_empty_verifiers,
    bench_budget_verifier,
    bench_turn_guard_adapter,
    bench_combined_verifiers,
    bench_snapshot_capture,
);
criterion_main!(benches);
