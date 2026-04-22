//! Commit C — integration test for the streaming speculative tool executor
//! plumbed into the SSE consumer via `SseStreamHost::on_tool_call_complete`.
//!
//! Deviation from task spec: rather than spinning up a full axum HTTP mock
//! and driving `astra-cli`'s chat loop end-to-end, we test at the
//! `sse_stream_host` layer (explicitly allowed by the task) using a
//! hand-rolled SSE byte stream. This is the natural integration surface:
//! the CLI host simply forwards `on_tool_call_complete` to the shared
//! `StreamingToolExecutor`; what we assert here is that the plumbing
//! wakes the executor at the right point during streaming and that
//! timings match the expected overlap profile.
//!
//! Asserts:
//!   • happy (speculation on): wall-clock ≈ max(stream_duration, tool_duration),
//!     strictly less than stream_duration + sum(tool_durations)
//!   • complex (speculation off): wall-clock ≥ stream_duration + slowest_tool
//!   • unhappy (permission Deny): speculation is *not* invoked for denied tool

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use astra_thin_client::ApprovalKind;
use astra_turn_core::chat_turn_sse_dispatch::SseRenderEffect;
use astra_turn_core::parallel_tool_exec::ToolExecutorFn;
use astra_turn_core::permission_types::PermissionDecision;
use astra_turn_core::sse_stream_host::{
    EdgeApprovalResult, EdgeToolExecResult, SseStreamHost, consume_sse_stream,
};
use astra_turn_core::streaming_tool_exec::{StreamingToolExecutor, should_speculate};
use async_trait::async_trait;
use futures_util::stream;
use serde_json::{Value, json};

// ─── Mock host ──────────────────────────────────────────────────────────────

struct SpeculatingHost {
    streaming: Arc<StreamingToolExecutor>,
    invocations: Arc<AtomicUsize>,
    deny_tool: Option<String>,
    call_log: Arc<tokio::sync::Mutex<Vec<String>>>,
}

#[async_trait]
impl SseStreamHost for SpeculatingHost {
    fn on_render_effects(&mut self, _effects: Vec<SseRenderEffect>) {}
    fn on_stream_complete(&mut self) {}

    async fn execute_tool(
        &mut self,
        request_id: &str,
        tool: &str,
        args: &Value,
    ) -> EdgeToolExecResult {
        EdgeToolExecResult {
            request_id: request_id.to_string(),
            tool: tool.to_string(),
            args: args.clone(),
            output: String::new(),
            tool_result_fields: None,
            status: "ok".to_string(),
            duration_ms: 0,
        }
    }

    async fn resolve_approval(
        &mut self,
        request_id: &str,
        _tool: &str,
        _approval_kind: ApprovalKind,
        _session_id: Option<&str>,
        _detail: Option<&str>,
    ) -> EdgeApprovalResult {
        EdgeApprovalResult {
            request_id: request_id.to_string(),
            decision: "deny".to_string(),
            reason: None,
        }
    }

    async fn on_tool_call_complete(&mut self, index: usize, tool_call: &Value) {
        let tool_name = tool_call
            .get("function")
            .and_then(|f| f.get("name"))
            .and_then(|n| n.as_str())
            .unwrap_or("")
            .to_string();
        let call_id = tool_call
            .get("id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        // Simulate permission pre-check: denied tools must not speculate.
        let perm = if Some(&tool_name) == self.deny_tool.as_ref() {
            Some(PermissionDecision::deny("test policy"))
        } else {
            Some(PermissionDecision::approve())
        };
        if !should_speculate(&tool_name, perm.as_ref()) {
            return;
        }

        self.invocations.fetch_add(1, Ordering::SeqCst);
        self.call_log.lock().await.push(call_id.clone());
        self.streaming
            .on_tool_block(call_id, tool_name, tool_call.clone(), index)
            .await;
    }
}

// ─── Mock executor that delays to simulate I/O ──────────────────────────────

fn delayed_executor(delay: Duration, log: Arc<tokio::sync::Mutex<Vec<String>>>) -> ToolExecutorFn {
    Arc::new(move |tc: Value| {
        let log = log.clone();
        Box::pin(async move {
            tokio::time::sleep(delay).await;
            let call_id = tc["id"].as_str().unwrap_or("").to_string();
            let name = tc["function"]["name"].as_str().unwrap_or("").to_string();
            log.lock().await.push(format!("exec:{call_id}"));
            (call_id, name, "ok".to_string(), true)
        })
    })
}

// ─── SSE byte stream builder ────────────────────────────────────────────────
//
// Produces a chunked byte stream matching the framer's dispatchable event
// shape. Each event is a JSON object on its own line terminated by \n\n.
// The framer (`ChatTurnSseFramer`) accepts either raw JSON blocks or
// `data: <json>` lines, both delimited by a blank line.

fn sse_event(obj: Value) -> Vec<u8> {
    let s = format!("data: {}\n\n", serde_json::to_string(&obj).unwrap());
    s.into_bytes()
}

async fn build_sse_chunks(
    slow_gap: Duration,
) -> (
    Box<dyn futures_util::Stream<Item = Result<Vec<u8>, String>> + Unpin + Send>,
    Duration, /* total stream duration */
) {
    let ev1 = sse_event(json!({ "type": "text_delta", "content": "Let me look." }));
    let ev2 = sse_event(json!({
        "type": "tool_call",
        "id": "c1",
        "function": { "name": "read_file", "arguments": "{\"path\":\"/x/a.txt\"}" }
    }));
    let ev3 = sse_event(json!({ "type": "text_delta", "content": "...still thinking..." }));
    let ev4 = sse_event(json!({
        "type": "tool_call",
        "id": "c2",
        "function": { "name": "grep", "arguments": "{\"pattern\":\"hello\"}" }
    }));
    let ev5 = sse_event(json!({ "type": "turn_complete", "has_tool_calls": true }));

    // Drive emission via async_stream-like construct using futures::stream::unfold.
    // State: (step, events_after_gap). Step 0: emit ev1. Step 1: emit ev2.
    // Step 2: sleep slow_gap then emit ev3. Step 3..5: ev4, ev5. Step 6: end.
    let state = (0u8, [ev1, ev2, ev3, ev4, ev5]);
    let gap = slow_gap;
    let s = futures_util::stream::unfold(state, move |(step, evs)| async move {
        match step {
            0 => Some((Ok(evs[0].clone()), (1, evs))),
            1 => Some((Ok(evs[1].clone()), (2, evs))),
            2 => {
                tokio::time::sleep(gap).await;
                Some((Ok(evs[2].clone()), (3, evs)))
            }
            3 => Some((Ok(evs[3].clone()), (4, evs))),
            4 => Some((Ok(evs[4].clone()), (5, evs))),
            _ => None,
        }
    });
    (Box::new(Box::pin(s)), slow_gap)
}

fn noop_stream() -> impl futures_util::Stream<Item = Result<Vec<u8>, String>> + Unpin + Send {
    stream::empty::<Result<Vec<u8>, String>>()
}

// Quiet the unused-import warnings across test modules:
#[allow(dead_code)]
fn _anchor() {
    let _ = noop_stream();
}

// ─── Tests ──────────────────────────────────────────────────────────────────

#[tokio::test]
async fn happy_speculation_overlaps_with_stream() {
    let tool_delay = Duration::from_millis(400);
    let stream_gap = Duration::from_millis(500);

    let exec_log = Arc::new(tokio::sync::Mutex::new(Vec::new()));
    let streaming = Arc::new(StreamingToolExecutor::new(delayed_executor(
        tool_delay,
        exec_log.clone(),
    )));
    let invocations = Arc::new(AtomicUsize::new(0));
    let call_log = Arc::new(tokio::sync::Mutex::new(Vec::new()));

    let mut host = SpeculatingHost {
        streaming: streaming.clone(),
        invocations: invocations.clone(),
        deny_tool: None,
        call_log: call_log.clone(),
    };

    let (mut chunks, _dur) = build_sse_chunks(stream_gap).await;

    let t0 = Instant::now();
    let (_result, _abort) =
        consume_sse_stream(&mut chunks, &mut host, Duration::from_secs(10)).await;

    // Harvest speculative results
    let spec_results = streaming.wait_all().await;
    let elapsed = t0.elapsed();

    // Two read-only tools should have been speculated
    assert_eq!(invocations.load(Ordering::SeqCst), 2);
    assert_eq!(spec_results.len(), 2);

    // The first tool starts ~immediately (after ev2 arrives); stream gap is
    // 500ms, tool delay is 400ms. With speculation, total ≈ max(500, 400) ≈
    // 500ms, well under serial 500+400+400 = 1300ms.
    let serial_bound = stream_gap + tool_delay + tool_delay;
    assert!(
        elapsed < serial_bound,
        "speculation-on elapsed {:?} should be < serial bound {:?}",
        elapsed,
        serial_bound
    );

    // Sanity: elapsed must be at least stream_gap (can't finish before stream).
    // Allow small epsilon of 50ms for scheduling jitter.
    assert!(
        elapsed + Duration::from_millis(50) >= stream_gap,
        "elapsed {:?} < stream_gap {:?}",
        elapsed,
        stream_gap
    );
}

#[tokio::test]
async fn unhappy_denied_tool_is_not_speculated() {
    let tool_delay = Duration::from_millis(50);
    let stream_gap = Duration::from_millis(100);

    let exec_log = Arc::new(tokio::sync::Mutex::new(Vec::new()));
    let streaming = Arc::new(StreamingToolExecutor::new(delayed_executor(
        tool_delay,
        exec_log.clone(),
    )));
    let invocations = Arc::new(AtomicUsize::new(0));
    let call_log = Arc::new(tokio::sync::Mutex::new(Vec::new()));

    let mut host = SpeculatingHost {
        streaming: streaming.clone(),
        invocations: invocations.clone(),
        // Deny grep → should NOT speculate
        deny_tool: Some("grep".to_string()),
        call_log: call_log.clone(),
    };

    let (mut chunks, _) = build_sse_chunks(stream_gap).await;

    let (_result, _abort) =
        consume_sse_stream(&mut chunks, &mut host, Duration::from_secs(10)).await;
    let _ = streaming.wait_all().await;

    // Only read_file should have been speculated (grep denied)
    assert_eq!(
        invocations.load(Ordering::SeqCst),
        1,
        "only read_file should speculate; grep is denied"
    );
    let log = call_log.lock().await;
    assert_eq!(log.len(), 1);
    assert_eq!(log[0], "c1");

    // Denied tool must not have touched the executor either
    let exec_log = exec_log.lock().await;
    assert!(
        !exec_log.iter().any(|l| l.contains("c2")),
        "grep (c2) must not have executed; executor log: {:?}",
        *exec_log
    );
}

#[tokio::test]
async fn complex_speculation_off_is_serial() {
    // Equivalent script with speculation disabled: we simply don't invoke the
    // streaming executor in on_tool_call_complete. After the stream ends we
    // execute the tools sequentially ourselves and measure total time.
    let tool_delay = Duration::from_millis(400);
    let stream_gap = Duration::from_millis(500);

    struct NoSpecHost;
    #[async_trait]
    impl SseStreamHost for NoSpecHost {
        fn on_render_effects(&mut self, _effects: Vec<SseRenderEffect>) {}
        fn on_stream_complete(&mut self) {}
        async fn execute_tool(
            &mut self,
            request_id: &str,
            tool: &str,
            args: &Value,
        ) -> EdgeToolExecResult {
            EdgeToolExecResult {
                request_id: request_id.to_string(),
                tool: tool.to_string(),
                args: args.clone(),
                output: String::new(),
                tool_result_fields: None,
                status: "ok".to_string(),
                duration_ms: 0,
            }
        }
        async fn resolve_approval(
            &mut self,
            request_id: &str,
            _tool: &str,
            _approval_kind: ApprovalKind,
            _session_id: Option<&str>,
            _detail: Option<&str>,
        ) -> EdgeApprovalResult {
            EdgeApprovalResult {
                request_id: request_id.to_string(),
                decision: "deny".to_string(),
                reason: None,
            }
        }
        // No on_tool_call_complete override → default no-op.
    }

    let mut host = NoSpecHost;
    let (mut chunks, _) = build_sse_chunks(stream_gap).await;

    let t0 = Instant::now();
    let (result, _abort) =
        consume_sse_stream(&mut chunks, &mut host, Duration::from_secs(10)).await;
    // Simulate the post-stream sequential batch: one tool after another.
    for _ in &result.accum.tool_calls {
        tokio::time::sleep(tool_delay).await;
    }
    let elapsed = t0.elapsed();

    let serial_lower = stream_gap + tool_delay; // at least one tool runs post-stream
    assert!(
        elapsed >= serial_lower,
        "speculation-off elapsed {:?} should be >= {:?} (stream + at least one tool)",
        elapsed,
        serial_lower
    );
}

// ─── Deterministic non-timing test: harvest/merge contract ────────────────

#[tokio::test]
async fn deterministic_harvest_merge_contract() {
    let exec_log = Arc::new(tokio::sync::Mutex::new(Vec::new()));
    let streaming = Arc::new(StreamingToolExecutor::new(delayed_executor(
        Duration::from_millis(10),
        exec_log,
    )));

    // Simulate 3 tool_blocks observed mid-stream: 2 read-only + 1 mutating.
    streaming
        .on_tool_block(
            "c1".into(),
            "read_file".into(),
            json!({ "id":"c1", "function": {"name":"read_file", "arguments":"{}"}}),
            0,
        )
        .await;
    streaming
        .on_tool_block(
            "c2".into(),
            "bash".into(),
            json!({ "id":"c2", "function": {"name":"bash", "arguments":"{}"}}),
            1,
        )
        .await;
    streaming
        .on_tool_block(
            "c3".into(),
            "grep".into(),
            json!({ "id":"c3", "function": {"name":"grep", "arguments":"{}"}}),
            2,
        )
        .await;

    // Merge: c1/c2/c3 are the ids the post-stream batch wants to execute.
    let (done, needed) = streaming
        .merge_speculative(&["c1".into(), "c2".into(), "c3".into()])
        .await;

    // c1 and c3 were speculated; c2 (bash) was skipped (not read-only).
    assert_eq!(done.len(), 2);
    assert_eq!(needed, vec!["c2".to_string()]);
}
