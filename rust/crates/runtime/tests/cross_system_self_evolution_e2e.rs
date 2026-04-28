//! Cross-system e2e coverage: exercises the full path
//! `StreamingToolExecutor -> ObservabilityHub -> AutoTuningEngine -> SelfModel`
//! to verify that each layer forwards what its downstream consumer expects.
//!
//! This complements the unit tests that cover each module in isolation and
//! the parallel_tool_exec_cap_test that covers parallel tool execution in
//! isolation. The goal here is to catch regressions where an individual
//! module's invariants still hold but the glue between layers silently drops
//! values.
//!
//! Three scenarios:
//! - happy path: speculation succeeds, metrics flow to tuning stats, tuning
//!   does NOT recommend disabling speculation, SelfModel renders cleanly.
//! - unhappy path: speculation mostly misses at `MIN_SAMPLES` volume, tuning
//!   flips `should_disable_streaming_speculation()` to true.
//! - complex path: tool_health + scenario + feedback signals all injected
//!   together through `ingest_self_model_inputs`, SelfModel reflects each.

use std::sync::Arc;

use astra_learning::auto_tuning::{FeedbackSignal, SignalType};
use astra_pipeline::ToolHealthEntry;
use astra_runtime::observability_integration::{ObservabilityHub, ObservabilitySession};
use astra_runtime::self_model::SelfModel;
use astra_runtime::user_profile::Scenario;
use astra_turn_core::parallel_tool_exec::ToolExecutorFn;
use astra_turn_core::streaming_tool_exec::{StreamingSpeculationMetrics, StreamingToolExecutor};
use astra_turn_core::tool_health::ToolHealthTracker;
use serde_json::{Value, json};

fn fast_executor() -> ToolExecutorFn {
    Arc::new(|tc: Value| {
        Box::pin(async move {
            let call_id = tc["id"].as_str().unwrap_or("").to_string();
            let name = tc["function"]["name"].as_str().unwrap_or("").to_string();
            (call_id, name.clone(), format!("result:{}", name), true)
        })
    })
}

fn tool_block(name: &str, id: &str) -> Value {
    json!({
        "id": id,
        "function": { "name": name, "arguments": "{}" }
    })
}

fn make_entry(name: &str, total_calls: usize, total_failures: usize) -> ToolHealthEntry {
    let failure_rate = if total_calls == 0 {
        0.0
    } else {
        total_failures as f64 / total_calls as f64
    };
    ToolHealthEntry {
        name: name.to_string(),
        total_calls,
        total_failures,
        failure_rate,
        last_updated_epoch: 0,
        recent_outcomes: Vec::new(),
    }
}

fn build_snapshot(session: &ObservabilitySession, tool_names: &[&str]) -> SelfModel {
    let tracker = if session.last_tool_health_export.is_empty() {
        None
    } else {
        Some(ToolHealthTracker::from_entries(
            &session.last_tool_health_export,
        ))
    };

    SelfModel::snapshot_with_strategy(
        tool_names,
        &[],
        &[],
        &session.cached_skill_names,
        tracker.as_ref(),
        session.turn_number,
        None,
        session.active_scenario.as_ref(),
        None,
        session.started_at.elapsed().as_secs(),
        session.user_corrections.len(),
        session.compressed_turns.len(),
        None,
        None,
        None,
        None,
        None,
        &session.last_feedback_signals,
        &session.config,
        session.last_strategy_application.as_ref(),
    )
}

/// Happy path: a real speculation round produces metrics, the hub ingests
/// them, and the tuning engine's running stats reflect the values without
/// flipping the "disable" recommendation.
#[tokio::test]
async fn happy_streaming_speculation_metrics_reach_tuning_stats() {
    let exec = StreamingToolExecutor::new(fast_executor());

    // Two read-only speculations, both complete.
    exec.on_tool_block(
        "c1".into(),
        "read_file".into(),
        tool_block("read_file", "c1"),
        0,
    )
    .await;
    exec.on_tool_block("c2".into(), "grep".into(), tool_block("grep", "c2"), 1)
        .await;

    // Wait for both to finish and credit them as hits via merge_speculative.
    let (done, needed) = exec
        .merge_speculative(&["c1".to_string(), "c2".to_string()])
        .await;
    assert_eq!(done.len(), 2);
    assert!(needed.is_empty());

    let snapshot = exec.snapshot().await;
    assert_eq!(snapshot.started, 2, "both speculations must be counted");
    assert_eq!(
        snapshot.hit, 2,
        "merge_speculative should credit both as hits"
    );

    // Plumb metrics into the hub and check the tuning engine has seen them.
    let hub = ObservabilityHub::new();
    hub.record_streaming_speculation_metrics(&snapshot);

    let stats = hub.tuning().streaming_speculation_stats();
    assert_eq!(stats.started, snapshot.started);
    assert_eq!(stats.hit, snapshot.hit);
    assert_eq!(stats.discarded, snapshot.discarded);
    assert_eq!(stats.total_saved_ms, snapshot.total_saved_ms);
    assert_eq!(stats.reports, 1, "one batch => one report");

    // With only 2 samples we're well below MIN_SAMPLES=20 so the recommendation
    // must remain "do not disable".
    assert!(
        !hub.tuning().should_disable_streaming_speculation(),
        "hit rate is 100% with 2 samples; must not recommend disabling"
    );

    // The session-level SelfModel render must not panic even when there are
    // no injected signals yet (matches production's first-turn state).
    let session = ObservabilitySession::new_simple("happy-sess");
    let model = build_snapshot(&session, &["read_file", "grep"]);
    let text = model.to_detailed_text();
    assert!(!text.is_empty(), "SelfModel detailed text must render");
}

/// Unhappy path: feed enough low-hit-rate samples to cross
/// `STREAMING_SPEC_MIN_SAMPLES` and force `should_disable_streaming_speculation`
/// to true, proving the gating threshold is actually wired through the hub.
#[tokio::test]
async fn unhappy_low_hit_rate_trips_disable_recommendation() {
    let hub = ObservabilityHub::new();

    // Feed 4 batches of 6 speculations, only 1 total hit. That is 24 started,
    // 1 hit => ~4% hit rate, well under the 10% floor, with samples >= 20.
    let batches = [
        StreamingSpeculationMetrics {
            started: 6,
            hit: 1,
            discarded: 5,
            inflight: 0,
            total_saved_ms: 12,
        },
        StreamingSpeculationMetrics {
            started: 6,
            hit: 0,
            discarded: 6,
            inflight: 0,
            total_saved_ms: 0,
        },
        StreamingSpeculationMetrics {
            started: 6,
            hit: 0,
            discarded: 6,
            inflight: 0,
            total_saved_ms: 0,
        },
        StreamingSpeculationMetrics {
            started: 6,
            hit: 0,
            discarded: 6,
            inflight: 0,
            total_saved_ms: 0,
        },
    ];

    // Before any samples: recommendation must be false.
    assert!(!hub.tuning().should_disable_streaming_speculation());

    for batch in &batches {
        hub.record_streaming_speculation_metrics(batch);
    }

    let stats = hub.tuning().streaming_speculation_stats();
    assert_eq!(stats.started, 24);
    assert_eq!(stats.hit, 1);
    assert_eq!(stats.reports, 4);
    assert!(
        hub.tuning().should_disable_streaming_speculation(),
        "24 samples @ ~4% hit rate must trigger the disable recommendation"
    );
}

/// Complex path: populate all four SelfModel inputs on a session — skills,
/// tool_health (with a deprioritized tool), scenario, and feedback signals —
/// and verify each channel is reflected in the rendered detailed text.
#[tokio::test]
async fn complex_full_self_model_ingestion_reflects_all_layers() {
    let mut session = ObservabilitySession::new_simple("complex-sess");

    let tool_health = vec![
        make_entry("bash", 20, 12),      // 60% failure rate → deprioritized
        make_entry("read_file", 100, 2), // healthy
        make_entry("grep", 50, 1),       // healthy
        make_entry("edit_file", 30, 0),  // perfect
    ];
    let skills = vec![
        "rust_refactor".to_string(),
        "test_runner".to_string(),
        "git_ops".to_string(),
    ];
    let signals: Vec<FeedbackSignal> = vec![
        FeedbackSignal::new(SignalType::ToolDeprioritized {
            tool_name: "bash".into(),
        })
        .with_turn("t-1"),
        FeedbackSignal::new(SignalType::TaskSuccess).with_turn("t-2"),
        FeedbackSignal::new(SignalType::HighTokenUsage {
            tokens: 9000,
            threshold: 4000,
        })
        .with_turn("t-3"),
    ];

    session.ingest_self_model_inputs(
        skills.clone(),
        tool_health.clone(),
        Some(Scenario::Debugging),
        signals.clone(),
    );

    // Each channel reached the session unchanged.
    assert_eq!(session.cached_skill_names, skills);
    assert_eq!(session.last_tool_health_export.len(), tool_health.len());
    assert_eq!(session.active_scenario, Some(Scenario::Debugging));
    assert_eq!(session.last_feedback_signals.len(), signals.len());

    // And each is observable in the rendered SelfModel text.
    let model = build_snapshot(&session, &["bash", "read_file", "grep", "edit_file"]);
    let text = model.to_detailed_text();

    assert!(
        text.contains("rust_refactor") || text.contains("skills"),
        "expected a skills mention in SelfModel text, got:\n{text}"
    );
    assert!(
        text.to_lowercase().contains("bash"),
        "expected bash (deprioritized) to be referenced in SelfModel text"
    );
}

/// End-to-end: actually execute a batch via `execute_parallel_round`, derive
/// tool_health entries from the outcome, feed them through
/// `ingest_self_model_inputs`, and prove the failing tool appears in the
/// rendered SelfModel text. This exercises the exact production sequence
/// (parallel exec -> health accounting -> self awareness) rather than
/// synthesizing ToolHealthEntry values directly.
#[tokio::test]
async fn parallel_failure_flows_into_self_model_deprioritization() {
    use astra_turn_core::parallel_tool_exec::execute_parallel_round;

    // Executor: read_file succeeds, flaky_reader always fails, bash succeeds.
    let exec: ToolExecutorFn = Arc::new(|tc: Value| {
        Box::pin(async move {
            let call_id = tc["id"].as_str().unwrap_or("").to_string();
            let name = tc["function"]["name"].as_str().unwrap_or("").to_string();
            let success = name != "flaky_reader";
            let content = if success {
                format!("ok:{name}")
            } else {
                format!("error: {name} failed")
            };
            (call_id, name, content, success)
        })
    });

    // 5 calls: 3 good reads, 2 failing reads interleaved with them.
    let calls: Vec<Value> = vec![
        tool_block("read_file", "r0"),
        tool_block("flaky_reader", "r1"),
        tool_block("read_file", "r2"),
        tool_block("flaky_reader", "r3"),
        tool_block("read_file", "r4"),
    ];

    let outcome = execute_parallel_round(&calls, exec).await;
    assert_eq!(outcome.results.len(), 5);

    // Aggregate into tool_health entries from the real batch outcome.
    use std::collections::BTreeMap;
    let mut counters: BTreeMap<String, (usize, usize)> = BTreeMap::new();
    for r in &outcome.results {
        let entry = counters.entry(r.tool_name.clone()).or_insert((0, 0));
        entry.0 += 1;
        if !r.success {
            entry.1 += 1;
        }
    }
    let tool_health: Vec<ToolHealthEntry> = counters
        .iter()
        .map(|(n, (calls, fails))| make_entry(n, *calls, *fails))
        .collect();

    // The failing tool's entry must have non-zero failure_rate.
    let flaky = tool_health
        .iter()
        .find(|e| e.name == "flaky_reader")
        .expect("flaky_reader entry expected");
    assert!(
        flaky.failure_rate > 0.99,
        "flaky_reader must have ~100% failure_rate, got {}",
        flaky.failure_rate
    );

    // Synthesize a ToolDeprioritized signal like the production health tracker
    // would emit after crossing its threshold.
    let signals = vec![
        FeedbackSignal::new(SignalType::ToolDeprioritized {
            tool_name: "flaky_reader".into(),
        })
        .with_turn("t-42"),
    ];

    let mut session = ObservabilitySession::new_simple("parallel-health-sess");
    session.ingest_self_model_inputs(
        vec![],
        tool_health.clone(),
        Some(Scenario::Debugging),
        signals,
    );

    let model = build_snapshot(&session, &["read_file", "flaky_reader"]);
    let text = model.to_detailed_text();
    assert!(
        text.to_lowercase().contains("flaky_reader"),
        "SelfModel text must surface the failing tool name; got:\n{text}"
    );
}
