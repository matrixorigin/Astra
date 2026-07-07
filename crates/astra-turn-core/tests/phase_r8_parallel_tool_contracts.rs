//! Phase-R8 adversarial contract pins for
//! [`astra_turn_core::parallel_tool_exec::execute_parallel_round`].
//!
//! These tests don't fix a bug — they lock in current behavior that's
//! easy to silently regress: panic isolation via `catch_unwind` /
//! `tokio::task::spawn`, read-only parallelism wall-time, first-mutation
//! sibling-abort on failure, and result-index preservation across mixed
//! batches.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use astra_turn_core::parallel_tool_exec::{ToolExecutorFn, execute_parallel_round};
use serde_json::{Value, json};

fn tc(name: &str, id: &str) -> Value {
    json!({
        "id": id,
        "type": "function",
        "function": { "name": name, "arguments": "{}" }
    })
}

/// Pin: a panic inside a Phase-1 (read-only) tool closure is turned into
/// a structured `ToolExecResult { success: false, content: "internal
/// error: task panicked: …" }` and the surrounding loop continues to
/// deliver results for sibling tools (it does NOT propagate the panic
/// out of `execute_parallel_round`).
#[tokio::test]
async fn phase1_tool_panic_is_captured_as_structured_error() {
    let executor: ToolExecutorFn = Arc::new(|tc_value: Value| {
        Box::pin(async move {
            let name = tc_value["function"]["name"]
                .as_str()
                .unwrap_or("")
                .to_string();
            let call_id = tc_value["id"].as_str().unwrap_or("").to_string();
            if name == "grep" {
                panic!("intentional panic in grep");
            }
            (call_id, name, "ok".into(), true)
        })
    });

    let calls = vec![
        tc("read_file", "a"),
        tc("grep", "b"), // panics
        tc("glob", "c"),
    ];
    let outcome = execute_parallel_round(&calls, executor).await;

    assert_eq!(outcome.results.len(), 3, "all three results present");
    // Order preservation.
    assert_eq!(outcome.results[0].original_index, 0);
    assert_eq!(outcome.results[1].original_index, 1);
    assert_eq!(outcome.results[2].original_index, 2);

    // Sibling (non-panicking) tools succeed.
    assert!(outcome.results[0].success, "read_file should succeed");
    assert!(outcome.results[2].success, "glob should succeed");

    // Panicking tool reports a structured failure.
    let panicked = &outcome.results[1];
    assert!(!panicked.success, "panicking tool must report failure");
    assert!(
        panicked.content.contains("task panicked"),
        "panic must be surfaced in content; got: {}",
        panicked.content
    );
}

/// Pin: three read-only tools each sleep ~200ms; if they run with shared
/// permits and actually parallelize (MAX_CONCURRENT_READ_ONLY >= 3) the
/// total wall time is well below 500ms.
#[tokio::test]
async fn phase1_read_only_tools_run_in_parallel_under_500ms() {
    let executor: ToolExecutorFn = Arc::new(|tc_value: Value| {
        Box::pin(async move {
            tokio::time::sleep(Duration::from_millis(200)).await;
            let name = tc_value["function"]["name"]
                .as_str()
                .unwrap_or("")
                .to_string();
            let call_id = tc_value["id"].as_str().unwrap_or("").to_string();
            (call_id, name, "ok".into(), true)
        })
    });

    let calls = vec![tc("read_file", "1"), tc("grep", "2"), tc("glob", "3")];
    let started = Instant::now();
    let outcome = execute_parallel_round(&calls, executor).await;
    let elapsed = started.elapsed();

    assert_eq!(outcome.parallel_count, 3);
    assert_eq!(outcome.sequential_count, 0);
    assert!(
        elapsed < Duration::from_millis(500),
        "expected parallel wall-time < 500ms, got {elapsed:?}"
    );
    assert!(
        elapsed >= Duration::from_millis(180),
        "expected at least ~200ms (one sleep in parallel), got {elapsed:?}"
    );
}

/// Pin: first failing mutation aborts remaining Phase-2 siblings; the
/// first mutation runs to completion (its result is persisted) and
/// subsequent mutations report a structured "Aborted" result.
#[tokio::test]
async fn phase2_first_mutation_fails_aborts_remaining_siblings() {
    let ran = Arc::new(AtomicUsize::new(0));
    let ran_c = ran.clone();
    let executor: ToolExecutorFn = Arc::new(move |tc_value: Value| {
        let ran = ran_c.clone();
        Box::pin(async move {
            ran.fetch_add(1, Ordering::SeqCst);
            let call_id = tc_value["id"].as_str().unwrap_or("").to_string();
            let name = tc_value["function"]["name"]
                .as_str()
                .unwrap_or("")
                .to_string();
            let success = name != "bash"; // first mutation (bash) fails
            (call_id, name, "x".into(), success)
        })
    });

    let calls = vec![
        tc("bash", "m1"),        // fails → triggers abort
        tc("write_file", "m2"),  // must be aborted
        tc("str_replace", "m3"), // must be aborted
    ];
    let outcome = execute_parallel_round(&calls, executor).await;

    assert!(outcome.sibling_aborted);
    assert_eq!(outcome.results.len(), 3);

    // First mutation actually executed.
    assert_eq!(outcome.results[0].tool_name, "bash");
    assert!(!outcome.results[0].success);

    // Siblings reported as aborted with the canonical prefix.
    for idx in [1usize, 2] {
        let r = &outcome.results[idx];
        assert!(!r.success, "aborted sibling must be marked failed");
        assert!(
            r.content.starts_with("Aborted:"),
            "aborted content must start with 'Aborted:' — got: {}",
            r.content
        );
    }

    // Only the first mutation was actually dispatched to the executor.
    assert_eq!(
        ran.load(Ordering::SeqCst),
        1,
        "executor must not be called for aborted siblings"
    );
}

/// Pin: result vector length == input length and ordering preserved,
/// regardless of completion order (mixed read-only + mutating batch).
#[tokio::test]
async fn mixed_batch_preserves_input_length_and_ordering() {
    // Varying sleeps so read-only tools complete out-of-order.
    let executor: ToolExecutorFn = Arc::new(|tc_value: Value| {
        Box::pin(async move {
            let name = tc_value["function"]["name"]
                .as_str()
                .unwrap_or("")
                .to_string();
            let call_id = tc_value["id"].as_str().unwrap_or("").to_string();
            // Read-only tools finish at different times → scrambles completion.
            let delay_ms: u64 = match name.as_str() {
                "read_file" => 150,
                "grep" => 10,
                "glob" => 80,
                _ => 0,
            };
            tokio::time::sleep(Duration::from_millis(delay_ms)).await;
            (call_id, name, "ok".into(), true)
        })
    });

    let calls = vec![
        tc("read_file", "idx0"),
        tc("bash", "idx1"),
        tc("grep", "idx2"),
        tc("write_file", "idx3"),
        tc("glob", "idx4"),
    ];
    let outcome = execute_parallel_round(&calls, executor).await;

    assert_eq!(outcome.results.len(), 5, "len(output) == len(input)");
    for (i, r) in outcome.results.iter().enumerate() {
        assert_eq!(r.original_index, i, "input-index preserved at position {i}");
        assert_eq!(
            r.call_id,
            format!("idx{i}"),
            "call_id aligns with original position {i}"
        );
    }
    assert_eq!(outcome.parallel_count, 3);
    assert_eq!(outcome.sequential_count, 2);
    assert!(!outcome.sibling_aborted);
}
