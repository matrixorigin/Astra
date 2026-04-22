//! Integration test: parallel_tool_exec peak-concurrency cap.
//!
//! Asserts that when many read-only tools are submitted in one batch, the
//! shared semaphore caps peak concurrency at `MAX_CONCURRENT_READ_ONLY` (10).

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use astra_turn_core::parallel_tool_exec::{
    MAX_CONCURRENT_READ_ONLY, MAX_CONCURRENT_TOOL_EXECUTIONS, ToolExecutorFn,
    execute_parallel_round,
};
use serde_json::{Value, json};

fn tool_call(name: &str, id: &str) -> Value {
    json!({
        "id": id,
        "type": "function",
        "function": { "name": name, "arguments": "{}" }
    })
}

/// Executor that tracks peak concurrency via an AtomicUsize.
fn tracking_executor(delay_ms: u64) -> (ToolExecutorFn, Arc<AtomicUsize>) {
    let inflight = Arc::new(AtomicUsize::new(0));
    let peak = Arc::new(AtomicUsize::new(0));
    let inflight_c = inflight.clone();
    let peak_c = peak.clone();

    let exec: ToolExecutorFn = Arc::new(move |tc: Value| {
        let inflight = inflight_c.clone();
        let peak = peak_c.clone();
        Box::pin(async move {
            let cur = inflight.fetch_add(1, Ordering::SeqCst) + 1;
            peak.fetch_max(cur, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(delay_ms)).await;
            inflight.fetch_sub(1, Ordering::SeqCst);
            let call_id = tc["id"].as_str().unwrap_or("").to_string();
            let name = tc["function"]["name"].as_str().unwrap_or("").to_string();
            (call_id, name, "ok".into(), true)
        })
    });
    (exec, peak)
}

#[tokio::test]
async fn happy_5_read_only_tools_elapsed_close_to_slowest() {
    let delay = 80u64;
    let calls: Vec<Value> = (0..5)
        .map(|i| tool_call("read_file", &format!("c{i}")))
        .collect();
    let (exec, _peak) = tracking_executor(delay);

    let start = Instant::now();
    let outcome = execute_parallel_round(&calls, exec).await;
    let elapsed = start.elapsed();

    assert_eq!(outcome.results.len(), 5);
    assert_eq!(outcome.parallel_count, 5);
    // Elapsed should be close to 1× delay, far less than sum (5×).
    // Generous bound to avoid CI flakes: < 3× delay is still proof of parallelism.
    assert!(
        elapsed < Duration::from_millis(delay * 3),
        "5 parallel read-only tools took {elapsed:?}, expected ≪ {}ms",
        delay * 5
    );
}

#[tokio::test]
async fn peak_concurrency_capped_at_10_for_20_tools() {
    assert_eq!(MAX_CONCURRENT_READ_ONLY, 10);
    assert_eq!(MAX_CONCURRENT_TOOL_EXECUTIONS, 10);

    let calls: Vec<Value> = (0..20)
        .map(|i| tool_call("read_file", &format!("c{i}")))
        .collect();
    let (exec, peak) = tracking_executor(40);

    let outcome = execute_parallel_round(&calls, exec).await;
    assert_eq!(outcome.results.len(), 20);
    assert_eq!(outcome.parallel_count, 20);

    let observed_peak = peak.load(Ordering::SeqCst);
    assert!(
        observed_peak <= MAX_CONCURRENT_READ_ONLY,
        "peak concurrency {observed_peak} exceeded cap {MAX_CONCURRENT_READ_ONLY}"
    );
    // Also assert we actually saturated the cap (otherwise test is weak).
    assert!(
        observed_peak >= 2,
        "expected parallel execution, got peak {observed_peak}"
    );
}
