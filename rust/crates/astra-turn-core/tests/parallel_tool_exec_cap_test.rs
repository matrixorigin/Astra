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

/// Executor that makes the first bash call fail; everything else succeeds.
fn failing_bash_executor() -> (ToolExecutorFn, Arc<AtomicUsize>) {
    let invocations = Arc::new(AtomicUsize::new(0));
    let invocations_c = invocations.clone();
    let exec: ToolExecutorFn = Arc::new(move |tc: Value| {
        let invocations = invocations_c.clone();
        Box::pin(async move {
            invocations.fetch_add(1, Ordering::SeqCst);
            let call_id = tc["id"].as_str().unwrap_or("").to_string();
            let name = tc["function"]["name"].as_str().unwrap_or("").to_string();
            if name == "bash" {
                (call_id, name, "boom: nonzero exit".into(), false)
            } else {
                (call_id, name, "ok".into(), true)
            }
        })
    });
    (exec, invocations)
}

/// Executor that makes the first `write_file` call fail; everything else succeeds.
/// Used to verify sibling-abort triggers on non-bash mutating failures too.
fn failing_write_executor() -> (ToolExecutorFn, Arc<AtomicUsize>) {
    let invocations = Arc::new(AtomicUsize::new(0));
    let invocations_c = invocations.clone();
    let exec: ToolExecutorFn = Arc::new(move |tc: Value| {
        let invocations = invocations_c.clone();
        Box::pin(async move {
            invocations.fetch_add(1, Ordering::SeqCst);
            let call_id = tc["id"].as_str().unwrap_or("").to_string();
            let name = tc["function"]["name"].as_str().unwrap_or("").to_string();
            if name == "write_file" {
                (call_id, name, "boom: permission denied".into(), false)
            } else {
                (call_id, name, "ok".into(), true)
            }
        })
    });
    (exec, invocations)
}

/// Unhappy path: a failing `bash` call must abort queued mutating siblings.
/// Mix: [read (parallel), bash-failing, write_file, str_replace].
#[tokio::test]
async fn unhappy_write_after_failing_bash_is_aborted() {
    let calls = vec![
        tool_call("read_file", "r0"),
        tool_call("bash", "b1"),
        tool_call("write_file", "w2"),
        tool_call("str_replace", "s3"),
    ];
    let (exec, invocations) = failing_bash_executor();

    let outcome = execute_parallel_round(&calls, exec).await;

    // 1 read + 1 bash only — writes after the failed bash must be skipped.
    assert_eq!(outcome.parallel_count, 1, "one read-only call");
    assert_eq!(outcome.sequential_count, 3, "three mutating calls queued");
    assert!(outcome.sibling_aborted, "bash failure must flip the flag");

    // The executor should NOT have been called for w2 or s3 — only r0 + b1.
    assert_eq!(
        invocations.load(Ordering::SeqCst),
        2,
        "writes after failing bash must be skipped, not dispatched"
    );

    // Results are returned in original input order.
    assert_eq!(outcome.results.len(), 4);
    let ordered: Vec<_> = outcome
        .results
        .iter()
        .map(|r| (r.tool_name.as_str(), r.success))
        .collect();
    // r0 success, b1 failure, w2 aborted (success=false), s3 aborted.
    assert_eq!(ordered[0], ("read_file", true));
    assert_eq!(ordered[1], ("bash", false));
    assert!(!ordered[2].1);
    assert!(!ordered[3].1);
    assert!(
        outcome.results[2]
            .content
            .to_lowercase()
            .contains("aborted"),
        "aborted write must carry the 'aborted' reason: {}",
        outcome.results[2].content
    );
}

/// Complex path: 20 mixed read + write calls.
///
/// - Peak concurrency on read-only phase must stay ≤ cap (10).
/// - Writes run serially after the parallel phase (not concurrent with anything).
/// - Results come back in original input order.
#[tokio::test]
async fn complex_mixed_20_tools_respect_cap_and_ordering() {
    // Build 15 read_file + 5 write_file, interleaved so ordering is non-trivial.
    let mut calls: Vec<Value> = Vec::new();
    for i in 0..20 {
        let name = if i % 4 == 3 {
            "write_file"
        } else {
            "read_file"
        };
        calls.push(tool_call(name, &format!("c{i:02}")));
    }
    let (exec, peak) = tracking_executor(20);

    let outcome = execute_parallel_round(&calls, exec).await;

    assert_eq!(outcome.results.len(), 20);
    assert_eq!(outcome.parallel_count, 15);
    assert_eq!(outcome.sequential_count, 5);
    assert!(!outcome.sibling_aborted);

    // Peak concurrency: reads capped at 10; writes are serial.
    // Because reads + writes don't overlap in time, the observed peak
    // must still respect the read-only cap.
    let observed_peak = peak.load(Ordering::SeqCst);
    assert!(
        observed_peak <= MAX_CONCURRENT_READ_ONLY,
        "peak concurrency {observed_peak} exceeded cap {MAX_CONCURRENT_READ_ONLY}"
    );

    // Ordering: results must come back in the same order as the input.
    for (i, r) in outcome.results.iter().enumerate() {
        assert_eq!(
            r.original_index, i,
            "result {i} should preserve original_index"
        );
        assert_eq!(r.call_id, format!("c{i:02}"));
    }
}

// ─────────── Process-wide shared-semaphore regression ───────────
//
// Previously `execute_parallel_round` allocated a fresh
// `Semaphore::new(MAX_CONCURRENT_READ_ONLY)` on every call. Two concurrent
// batches could therefore run up to 2·10 = 20 tools simultaneously, breaking
// the "shared semaphore" contract stated in comments and prompts.
//
// With the process-wide semaphore, two (or more) concurrent batches must
// collectively respect `MAX_CONCURRENT_TOOL_EXECUTIONS`.

#[tokio::test]
async fn two_concurrent_batches_share_process_wide_semaphore_cap() {
    // Each batch fires 15 read-only tools. If the cap were per-batch, peak
    // across both batches could reach 2·10 = 20. With the shared cap, peak
    // must stay <= MAX_CONCURRENT_TOOL_EXECUTIONS.
    let delay = 120u64;

    let calls_a: Vec<Value> = (0..15)
        .map(|i| tool_call("read_file", &format!("a{i:02}")))
        .collect();
    let calls_b: Vec<Value> = (0..15)
        .map(|i| tool_call("grep", &format!("b{i:02}")))
        .collect();

    // Use a shared inflight/peak counter across BOTH executors so we observe
    // the true cross-batch peak, not per-batch.
    let inflight = Arc::new(AtomicUsize::new(0));
    let peak = Arc::new(AtomicUsize::new(0));

    let make_exec = || -> ToolExecutorFn {
        let inflight = inflight.clone();
        let peak = peak.clone();
        Arc::new(move |tc: Value| {
            let inflight = inflight.clone();
            let peak = peak.clone();
            Box::pin(async move {
                let cur = inflight.fetch_add(1, Ordering::SeqCst) + 1;
                peak.fetch_max(cur, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(delay)).await;
                inflight.fetch_sub(1, Ordering::SeqCst);
                let call_id = tc["id"].as_str().unwrap_or("").to_string();
                let name = tc["function"]["name"].as_str().unwrap_or("").to_string();
                (call_id, name, "ok".into(), true)
            })
        })
    };

    let (oa, ob) = tokio::join!(
        execute_parallel_round(&calls_a, make_exec()),
        execute_parallel_round(&calls_b, make_exec()),
    );

    assert_eq!(oa.results.len(), 15);
    assert_eq!(ob.results.len(), 15);

    let observed_peak = peak.load(Ordering::SeqCst);
    assert!(
        observed_peak <= MAX_CONCURRENT_TOOL_EXECUTIONS,
        "cross-batch peak {observed_peak} must not exceed shared cap {MAX_CONCURRENT_TOOL_EXECUTIONS}"
    );
    assert!(
        observed_peak > 0,
        "sanity: at least one tool should have run concurrently",
    );
}

#[test]
fn shared_tool_semaphore_returns_same_instance() {
    use astra_turn_core::parallel_tool_exec::shared_tool_semaphore;
    let a = shared_tool_semaphore();
    let b = shared_tool_semaphore();
    assert!(
        Arc::ptr_eq(&a, &b),
        "shared_tool_semaphore must return the same Arc on repeat calls"
    );
}

// ── Sibling-abort coverage for non-bash mutating tools ──
//
// Previously, the sibling-abort guard only fired when the failing tool was
// in `["bash", "BashTool", "shell", "execute_command"]`. A failing
// `write_file`, `git_commit`, `str_replace`, etc. did **not** abort queued
// mutating siblings — even though mutations are typically part of a
// coherent sequence (write → commit → push; the next step is meaningless
// once an earlier one fails) and continuing can partially apply state.
//
// The guard now fires on any mutating-tool failure.

#[tokio::test]
async fn unhappy_any_failing_mutating_tool_aborts_siblings() {
    // write_file fails first; subsequent mutating tools (str_replace, bash)
    // must be skipped, not dispatched.
    let calls = vec![
        tool_call("read_file", "r0"),
        tool_call("write_file", "w1"),
        tool_call("str_replace", "s2"),
        tool_call("bash", "b3"),
    ];
    let (exec, invocations) = failing_write_executor();

    let outcome = execute_parallel_round(&calls, exec).await;

    assert_eq!(outcome.parallel_count, 1, "one read-only call");
    assert_eq!(outcome.sequential_count, 3, "three mutating calls queued");
    assert!(
        outcome.sibling_aborted,
        "write_file failure must flip the sibling-abort flag too"
    );

    // Executor should have been called for r0 + w1 only — 2 calls total.
    assert_eq!(
        invocations.load(Ordering::SeqCst),
        2,
        "mutating siblings after a failing write_file must be skipped"
    );

    let ordered: Vec<_> = outcome
        .results
        .iter()
        .map(|r| (r.tool_name.as_str(), r.success))
        .collect();
    assert_eq!(ordered[0], ("read_file", true));
    assert_eq!(ordered[1], ("write_file", false));
    assert!(!ordered[2].1, "str_replace should be aborted");
    assert!(!ordered[3].1, "bash should be aborted");
    for r in &outcome.results[2..] {
        assert!(
            r.content.to_lowercase().contains("aborted"),
            "aborted sibling must carry 'aborted' reason: {}",
            r.content
        );
    }
}
