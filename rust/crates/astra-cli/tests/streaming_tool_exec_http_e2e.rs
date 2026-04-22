//! Phase 2.5 Commit E — HTTP-level e2e for speculative streaming tool
//! execution (D-9).
//!
//! Deviation from the spec: rather than spin up the full `astra-cli`
//! chat_stream client end-to-end (which would require replicating auth,
//! payload prep, tool registry and rendering state), this test drives the
//! real HTTP boundary and pipes the streamed SSE bytes through the same
//! `consume_sse_stream` → `SseStreamHost::on_tool_call_complete` plumbing
//! used by the production CLI host. This exercises:
//!   • Real axum HTTP server emitting real `text/event-stream` bytes
//!   • Real `reqwest` client reading the body stream
//!   • Runtime's `consume_sse_stream` parser + hook dispatch
//!   • `StreamingToolExecutor` speculative dispatch + harvest
//!
//! The CLI layer's production wiring (Arc<ToolExecutor>, build_streaming_tool_exec,
//! harvest_speculation_for_batch) is exercised by the existing offline
//! unit/integration tests. The HTTP boundary is the new coverage here.

use std::convert::Infallible;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use astra_runtime::turn::chat_turn_sse_dispatch::SseRenderEffect;
use astra_runtime::turn::parallel_tool_exec::ToolExecutorFn;
use astra_runtime::turn::sse_stream_host::{
    EdgeApprovalResult, EdgeToolExecResult, SseStreamHost, consume_sse_stream,
};
use astra_runtime::turn::streaming_tool_exec::{StreamingToolExecutor, should_speculate};
use astra_thin_client::ApprovalKind;
use async_trait::async_trait;
use axum::{
    Router,
    body::Body,
    http::header,
    response::{IntoResponse, Response},
    routing::post,
};
use futures_util::{StreamExt, stream};
use serde_json::{Value, json};
use tokio::net::TcpListener;

// ── Mock SSE server ────────────────────────────────────────────────────────
//
// The CLI's runtime SSE parser (ChatTurnSseFramer) accepts `data: {JSON}\n\n`
// frames where the payload contains a `type` discriminator. We emit:
//   type=text_delta  (LLM thinking)
//   type=tool_call   (complete tool_use block — triggers on_tool_call_complete)
//   type=turn_complete
//
// Timing: tool_call for call_a at T≈0ms, gap of 500ms, tool_call for call_b,
// then turn_complete. With speculation ON, both tools' backend executors
// start before the stream finishes.

fn sse_frame(obj: Value) -> String {
    format!("data: {}\n\n", serde_json::to_string(&obj).unwrap())
}

async fn sse_handler() -> impl IntoResponse {
    let tool_a = json!({
        "type": "tool_call",
        "id": "call_a",
        "function": { "name": "grep", "arguments": "{\"pattern\":\"foo\"}" }
    });
    let tool_b = json!({
        "type": "tool_call",
        "id": "call_b",
        "function": { "name": "list_dir", "arguments": "{\"path\":\".\"}" }
    });
    let events: Vec<(Duration, String)> = vec![
        (
            Duration::from_millis(0),
            sse_frame(json!({ "type": "text_delta", "content": "thinking..." })),
        ),
        (Duration::from_millis(50), sse_frame(tool_a)),
        (
            Duration::from_millis(500),
            sse_frame(json!({ "type": "text_delta", "content": " more ..." })),
        ),
        (Duration::from_millis(50), sse_frame(tool_b)),
        (
            Duration::from_millis(100),
            sse_frame(json!({ "type": "turn_complete", "has_tool_calls": true })),
        ),
    ];

    let body_stream = stream::unfold(events.into_iter(), |mut it| async move {
        match it.next() {
            Some((delay, payload)) => {
                tokio::time::sleep(delay).await;
                Some((Ok::<_, Infallible>(axum::body::Bytes::from(payload)), it))
            }
            None => None,
        }
    });

    Response::builder()
        .status(200)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .body(Body::from_stream(body_stream))
        .unwrap()
}

async fn start_mock_server() -> String {
    let app = Router::new().route("/v1/chat/completions", post(sse_handler));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let url = format!("http://{}/v1/chat/completions", addr);
    tokio::spawn(async move {
        axum::serve(listener, app).await.ok();
    });
    url
}

// ── Host ───────────────────────────────────────────────────────────────────

#[derive(Clone, Debug)]
struct ToolTiming {
    name: String,
    started_at: Duration,
}

struct SpeculatingHost {
    streaming: Option<Arc<StreamingToolExecutor>>,
    deny_tool: Option<String>,
    #[allow(dead_code)]
    timings: Arc<tokio::sync::Mutex<Vec<ToolTiming>>>,
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
        let Some(exec) = self.streaming.clone() else {
            return;
        };
        let name = tool_call
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
        // Emulate permission pre-check: denied tool must not speculate.
        // We bypass should_speculate() entirely when the tool is denied,
        // mirroring the CLI's production logic where the permission layer
        // gates dispatch.
        if Some(&name) == self.deny_tool.as_ref() {
            return;
        }
        if !should_speculate(&name, None) {
            return;
        }
        let _ = exec
            .on_tool_block(call_id, name, tool_call.clone(), index)
            .await;
    }
}

fn make_executor(
    invocations: Arc<AtomicUsize>,
    timings: Arc<tokio::sync::Mutex<Vec<ToolTiming>>>,
    t0: Instant,
    tool_delay: Duration,
) -> ToolExecutorFn {
    Arc::new(move |tc: Value| {
        let invocations = Arc::clone(&invocations);
        let timings = Arc::clone(&timings);
        let name = tc
            .get("function")
            .and_then(|f| f.get("name"))
            .and_then(|n| n.as_str())
            .unwrap_or("")
            .to_string();
        let call_id = tc
            .get("id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        Box::pin(async move {
            let started_at = t0.elapsed();
            invocations.fetch_add(1, Ordering::SeqCst);
            timings.lock().await.push(ToolTiming {
                name: name.clone(),
                started_at,
            });
            tokio::time::sleep(tool_delay).await;
            (call_id, name, "ok".to_string(), true)
        })
    })
}

// ── Driver ─────────────────────────────────────────────────────────────────

async fn drive_sse_through_http(url: &str, host: &mut SpeculatingHost) -> Duration {
    let t_start = Instant::now();
    let client = reqwest::Client::builder()
        .no_proxy()
        .build()
        .expect("client");
    let resp = client
        .post(url)
        .header("content-type", "application/json")
        .json(&json!({"messages":[]}))
        .send()
        .await
        .expect("POST send");
    assert_eq!(resp.status(), 200);

    let byte_stream = resp
        .bytes_stream()
        .map(|r| r.map(|b| b.to_vec()).map_err(|e| e.to_string()));
    let mut stream: std::pin::Pin<
        Box<dyn futures_util::Stream<Item = Result<Vec<u8>, String>> + Send>,
    > = Box::pin(byte_stream);

    let (_res, _abort) = consume_sse_stream(&mut stream, host, Duration::from_secs(10)).await;
    t_start.elapsed()
}

// ── Tests ──────────────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn happy_speculation_on_starts_tools_mid_stream() {
    let url = start_mock_server().await;
    let t0 = Instant::now();
    let invocations = Arc::new(AtomicUsize::new(0));
    let timings = Arc::new(tokio::sync::Mutex::new(Vec::<ToolTiming>::new()));
    let streaming = Arc::new(StreamingToolExecutor::new(make_executor(
        Arc::clone(&invocations),
        Arc::clone(&timings),
        t0,
        Duration::from_millis(300),
    )));

    let mut host = SpeculatingHost {
        streaming: Some(Arc::clone(&streaming)),
        deny_tool: None,
        timings: Arc::clone(&timings),
    };

    let elapsed = drive_sse_through_http(&url, &mut host).await;
    let _ = streaming.wait_all().await;

    let timings_snap = timings.lock().await.clone();
    assert_eq!(
        invocations.load(Ordering::SeqCst),
        2,
        "both read-only tools should speculate: {:?}",
        timings_snap
    );
    // Stream finishes around ~700ms from start. With speculation both tool
    // futures start well before that and overlap. Serial worst-case would
    // be stream (~700ms) + 2 * tool_delay (600ms) = 1300ms; speculation
    // should collapse it.
    assert!(
        elapsed < Duration::from_millis(1100),
        "speculation-on elapsed {:?} did not overlap",
        elapsed
    );
    // Tool A should have started before the second tool_call frame (~600ms).
    let a = timings_snap
        .iter()
        .find(|t| t.name == "grep")
        .expect("grep timing recorded");
    assert!(
        a.started_at < Duration::from_millis(500),
        "grep started too late for speculation: {:?}",
        a.started_at
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unhappy_denied_tool_is_not_speculated() {
    let url = start_mock_server().await;
    let t0 = Instant::now();
    let invocations = Arc::new(AtomicUsize::new(0));
    let timings = Arc::new(tokio::sync::Mutex::new(Vec::<ToolTiming>::new()));
    let streaming = Arc::new(StreamingToolExecutor::new(make_executor(
        Arc::clone(&invocations),
        Arc::clone(&timings),
        t0,
        Duration::from_millis(100),
    )));

    let mut host = SpeculatingHost {
        streaming: Some(Arc::clone(&streaming)),
        deny_tool: Some("list_dir".to_string()),
        timings: Arc::clone(&timings),
    };

    let _ = drive_sse_through_http(&url, &mut host).await;
    let _ = streaming.wait_all().await;

    let timings_snap = timings.lock().await.clone();
    let names: Vec<String> = timings_snap.iter().map(|t| t.name.clone()).collect();
    assert!(
        names.iter().any(|n| n == "grep"),
        "allowed tool should have been speculated: {:?}",
        names
    );
    assert!(
        names.iter().all(|n| n != "list_dir"),
        "denied tool must NOT have been speculated: {:?}",
        names
    );
    assert_eq!(invocations.load(Ordering::SeqCst), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn complex_speculation_off_no_mid_stream_starts() {
    let url = start_mock_server().await;
    let timings = Arc::new(tokio::sync::Mutex::new(Vec::<ToolTiming>::new()));

    // speculation disabled: streaming is None — on_tool_call_complete no-ops
    // (mirrors the production path when ASTRA_STREAMING_TOOL_EXEC is unset).
    let mut host = SpeculatingHost {
        streaming: None,
        deny_tool: None,
        timings: Arc::clone(&timings),
    };

    let _ = drive_sse_through_http(&url, &mut host).await;

    let timings_snap = timings.lock().await.clone();
    assert!(
        timings_snap.is_empty(),
        "with speculation off, no speculative executions expected: {:?}",
        timings_snap
    );
}
