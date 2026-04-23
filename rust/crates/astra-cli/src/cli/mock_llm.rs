//! Mock LLM server for end-to-end testing of team orchestration without real LLM calls.
//!
//! Starts an in-process axum HTTP server that responds to `POST /chat/turn` with
//! pre-scripted SSE streams. Every other part of the system (worktrees, progress
//! channels, delegation engine, tool execution, journal) runs for real.
//!
//! # Usage
//! ```
//! /team run dev "build login page" --mock complete
//! /team run dev "build login page" --mock tool_then_complete
//! /team run dev "build login page" --mock multi_turn
//! /team run dev "build login page" --mock fail
//! /team run dev "build login page" --mock slow
//! ```

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::Response;
use axum::routing::post;
use serde_json::Value;
use tokio::net::TcpListener;

// ─── Scenario ────────────────────────────────────────────────────────────────

/// A named scenario that determines what SSE stream the mock server returns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MockScenario {
    /// Agent immediately outputs a completion message. No tool calls.
    Complete,
    /// Agent calls one edge tool (write_file), then completes.
    ToolThenComplete,
    /// Agent makes two LLM turns: first asks a question, second completes.
    MultiTurn,
    /// Agent returns an error response.
    Fail,
    /// Agent delays 3s before responding (tests timeout/progress display).
    Slow,
    /// Adversarial: a single tool_call_start event's JSON is split across
    /// multiple SSE `data:` chunks (with blank lines in between) so a naive
    /// per-chunk JSON decoder will fail on each half. A correct client must
    /// reassemble across SSE frames before parsing.
    SseChunkSplit,
    /// Adversarial: emits an SSE event whose JSON payload is malformed
    /// (unterminated string). A correct client must skip the bad frame and
    /// still deliver the surrounding valid frames (session_info → done).
    MalformedJson,
    /// Adversarial: the HTTP response is NOT 200. Returns 429 Too Many
    /// Requests with a Retry-After header and a JSON error body. A correct
    /// client must NOT panic parsing SSE from a non-200 response.
    RateLimited,
    /// Inference-only: the model returns text only, no tool_calls, even in
    /// a context where tool use was expected. Verifies callers do not
    /// assume tool_calls are always present and still finalize cleanly.
    TextOnly,
}

impl MockScenario {
    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "complete" => Some(Self::Complete),
            "tool_then_complete" | "tool" => Some(Self::ToolThenComplete),
            "multi_turn" | "multi" => Some(Self::MultiTurn),
            "fail" | "error" => Some(Self::Fail),
            "slow" => Some(Self::Slow),
            "sse_chunk_split" | "sse_split" | "chunk_split" => Some(Self::SseChunkSplit),
            "malformed_json" | "malformed" | "bad_json" => Some(Self::MalformedJson),
            "rate_limited" | "rate_limit" | "429" => Some(Self::RateLimited),
            "text_only" | "inference_only" | "no_tools" => Some(Self::TextOnly),
            _ => None,
        }
    }

    pub fn description(self) -> &'static str {
        match self {
            Self::Complete => "immediate completion (no tools)",
            Self::ToolThenComplete => "one write_file tool call, then completion",
            Self::MultiTurn => "two LLM turns: think then complete",
            Self::Fail => "agent returns error",
            Self::Slow => "3s delay before response (tests progress display)",
            Self::SseChunkSplit => "tool_call JSON split across SSE frames (adversarial)",
            Self::MalformedJson => "one SSE event carries malformed JSON (adversarial)",
            Self::RateLimited => "HTTP 429 with Retry-After (adversarial)",
            Self::TextOnly => "text completion only, no tool_calls (inference-only)",
        }
    }

    pub fn all() -> &'static [(&'static str, &'static str)] {
        &[
            ("complete", "immediate completion (no tools)"),
            (
                "tool_then_complete",
                "one write_file tool call, then completion",
            ),
            ("multi_turn", "two LLM turns: think then complete"),
            ("fail", "agent returns error"),
            ("slow", "3s delay before response (tests progress display)"),
            (
                "sse_chunk_split",
                "tool_call JSON split across SSE frames (adversarial)",
            ),
            (
                "malformed_json",
                "one SSE event carries malformed JSON (adversarial)",
            ),
            ("rate_limited", "HTTP 429 with Retry-After (adversarial)"),
            ("text_only", "text completion only, no tool_calls"),
        ]
    }
}

// ─── SSE helpers ─────────────────────────────────────────────────────────────

fn sse_line(event: &Value) -> String {
    format!("data: {}\n\n", event)
}

fn session_info(run_id: &str) -> String {
    sse_line(&serde_json::json!({
        "type": "session_info",
        "session_id": "mock-session",
        "run_id": run_id,
    }))
}

fn text_delta(content: &str) -> String {
    sse_line(&serde_json::json!({
        "type": "text_delta",
        "content": content,
    }))
}

fn text_done(full: &str) -> String {
    sse_line(&serde_json::json!({
        "type": "text_done",
        "full_text": full,
    }))
}

fn tool_call_start(call_id: &str, tool: &str, args: Value) -> String {
    // Canonical tool_call_start shape: flat `tool` (name string) + top-level
    // `arguments` (JSON-stringified). See
    // `chat_turn_sse_dispatch::normalize_tool_call_for_accum` — a nested
    // `tool: {name, arguments}` would be silently dropped because that
    // normalizer reads `tool` with `as_str()` and falls back to "" on a
    // non-string, returning None. The regression anchor is
    // `phase_r2_mock_dispatch_contract::mock_llm_tool_call_start_shape_is_captured_by_dispatch`.
    sse_line(&serde_json::json!({
        "type": "tool_call_start",
        "call_id": call_id,
        "tool": tool,
        "arguments": args.to_string(),
    }))
}

fn tool_result(call_id: &str, result: &str) -> String {
    sse_line(&serde_json::json!({
        "type": "tool_result",
        "call_id": call_id,
        "result": result,
    }))
}

fn done_event(tokens: u64) -> String {
    sse_line(&serde_json::json!({
        "type": "done",
        "tokens_used": tokens,
        "usage": { "prompt_tokens": tokens, "completion_tokens": 50 },
    }))
}

fn error_event(msg: &str) -> String {
    sse_line(&serde_json::json!({
        "type": "error",
        "message": msg,
        "code": "mock_error",
        "retryable": false,
    }))
}

// ─── Scenario bodies ─────────────────────────────────────────────────────────

fn body_complete(agent_id: &str, turn: u32) -> String {
    let msg = format!(
        "Task completed by {agent_id} (turn {turn}). \
         I have finished the assigned work successfully."
    );
    let mut s = String::new();
    s.push_str(&session_info(&format!("mock-run-{turn}")));
    s.push_str(&text_delta(&msg));
    s.push_str(&text_done(&msg));
    s.push_str(&done_event(200));
    s
}

fn body_tool_then_complete(agent_id: &str, turn: u32) -> String {
    let path = format!("/tmp/mock-output-{agent_id}-{turn}.txt");
    let content = format!("Output from {agent_id} turn {turn}");
    let mut s = String::new();
    s.push_str(&session_info(&format!("mock-run-{turn}")));
    // First: tool call
    s.push_str(&tool_call_start(
        "call-1",
        "write_file",
        serde_json::json!({ "path": path, "content": content }),
    ));
    s.push_str(&tool_result("call-1", &format!("Written {}", path)));
    // Then: completion text
    let msg = format!("{agent_id}: wrote {path} and completed task.");
    s.push_str(&text_delta(&msg));
    s.push_str(&text_done(&msg));
    s.push_str(&done_event(350));
    s
}

fn body_multi_turn(agent_id: &str, turn: u32) -> String {
    // Turn 1: thinking text; Turn 2+: completion
    if turn == 1 {
        let msg = format!("{agent_id}: analyzing the task requirements...");
        let mut s = String::new();
        s.push_str(&session_info("mock-run-multi"));
        s.push_str(&text_delta(&msg));
        s.push_str(&text_done(&msg));
        s.push_str(&done_event(150));
        s
    } else {
        let msg = format!("{agent_id}: analysis complete. Task finished on turn {turn}.");
        let mut s = String::new();
        s.push_str(&session_info("mock-run-multi"));
        s.push_str(&text_delta(&msg));
        s.push_str(&text_done(&msg));
        s.push_str(&done_event(200));
        s
    }
}

fn body_fail(_agent_id: &str, turn: u32) -> String {
    let mut s = String::new();
    s.push_str(&session_info(&format!("mock-run-fail-{turn}")));
    s.push_str(&error_event(
        "Mock agent failure: simulated LLM error for testing",
    ));
    s
}

async fn body_slow(agent_id: &str, turn: u32) -> String {
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    body_complete(agent_id, turn)
}

// ─── Adversarial bodies (P1 hardening) ──────────────────────────────────────

/// Split one `tool_call_start` event across multiple SSE frames.
///
/// A compliant SSE client MUST reassemble `data:` lines within one event
/// (separated by `\n`, terminated by `\n\n`). We abuse this by emitting:
///
/// ```text
/// data: {"type":"tool_call_start","call_id":"call-1","tool":{"name":"write_file","argume
/// data: nts":"{\"path\":\"/tmp/x\",\"content\":\"hi\"}"}}
///
/// ```
///
/// Both halves are under one SSE event (one blank line at the end) so a
/// correct parser joins them before JSON-decoding. A naive per-line decoder
/// will fail twice and miss the tool call entirely.
fn body_sse_chunk_split(agent_id: &str, turn: u32) -> String {
    let path = format!("/tmp/split-{agent_id}-{turn}.txt");
    let full_json = serde_json::json!({
        "type": "tool_call_start",
        "call_id": "call-split",
        "tool": {
            "name": "write_file",
            "arguments": serde_json::json!({
                "path": path,
                "content": "chunked payload"
            }).to_string()
        }
    })
    .to_string();

    // Split between fields at a comma so the SSE reassembly newline lands
    // on JSON whitespace (where it is legal); each half is still invalid
    // JSON on its own (an unbalanced object).
    let split_at = full_json
        .match_indices("\",\"")
        .nth(1) // second field boundary: after call_id
        .map(|(i, _)| i + 2) // include the `,`
        .unwrap_or(full_json.len() / 2);
    let (left, right) = full_json.split_at(split_at);

    let mut s = String::new();
    s.push_str(&session_info(&format!("mock-run-split-{turn}")));
    // Two `data:` lines, ONE blank terminator = one logical SSE event.
    s.push_str(&format!("data: {left}\ndata: {right}\n\n"));
    s.push_str(&tool_result("call-split", &format!("Written {path}")));
    let msg = format!("{agent_id}: completed after chunk-split tool call.");
    s.push_str(&text_delta(&msg));
    s.push_str(&text_done(&msg));
    s.push_str(&done_event(250));
    s
}

/// Emit a valid session_info, then a deliberately malformed JSON event, then
/// valid text_done + done frames. A robust parser must skip the broken frame
/// and still deliver a turn-complete signal.
fn body_malformed_json(agent_id: &str, turn: u32) -> String {
    let mut s = String::new();
    s.push_str(&session_info(&format!("mock-run-bad-{turn}")));
    // Unterminated string literal: no closing `"` before the newline.
    s.push_str("data: {\"type\":\"text_delta\",\"content\":\"unterminated\n\n");
    let msg = format!("{agent_id}: recovered after malformed frame (turn {turn}).");
    s.push_str(&text_done(&msg));
    s.push_str(&done_event(100));
    s
}

/// Body for the 429 Rate-Limited response. The mock handler uses this only
/// to fill the HTTP body — the status and Retry-After header are set by
/// `handle_chat_turn` so the non-200 path is exercised end-to-end.
fn body_rate_limited(turn: u32) -> String {
    serde_json::json!({
        "error": {
            "type": "rate_limit_error",
            "message": format!("mock rate limit (turn {turn})"),
            "retry_after_seconds": 1
        }
    })
    .to_string()
}

/// Inference-only: text completion with NO tool_call_start events.
/// Even if a caller passed tool schemas in the request, the model is free
/// to answer with text. This pins that callers don't treat "no tool calls"
/// as an error.
fn body_text_only(agent_id: &str, turn: u32) -> String {
    let msg = format!(
        "{agent_id}: answering directly without tools on turn {turn}. \
         No write_file, no read_file — pure inference."
    );
    let mut s = String::new();
    s.push_str(&session_info(&format!("mock-run-textonly-{turn}")));
    s.push_str(&text_delta(&msg));
    s.push_str(&text_done(&msg));
    s.push_str(&done_event(75));
    s
}

// ─── Server state ─────────────────────────────────────────────────────────────

#[derive(Clone)]
struct ServerState {
    scenario: MockScenario,
    call_count: Arc<AtomicU32>,
}

async fn handle_chat_turn(
    State(state): State<ServerState>,
    body: axum::body::Bytes,
) -> Response<axum::body::Body> {
    let turn = state.call_count.fetch_add(1, Ordering::Relaxed) + 1;

    // Extract agent_id from top-level payload field (set by chat_turn_base_payload)
    let agent_id = serde_json::from_slice::<Value>(&body)
        .ok()
        .and_then(|v| {
            v.get("agent_id")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .unwrap_or_else(|| format!("agent-{turn}"));

    let sse_body = match state.scenario {
        MockScenario::Complete => body_complete(&agent_id, turn),
        MockScenario::ToolThenComplete => body_tool_then_complete(&agent_id, turn),
        MockScenario::MultiTurn => body_multi_turn(&agent_id, turn),
        MockScenario::Fail => body_fail(&agent_id, turn),
        MockScenario::Slow => body_slow(&agent_id, turn).await,
        MockScenario::SseChunkSplit => body_sse_chunk_split(&agent_id, turn),
        MockScenario::MalformedJson => body_malformed_json(&agent_id, turn),
        MockScenario::TextOnly => body_text_only(&agent_id, turn),
        MockScenario::RateLimited => {
            return Response::builder()
                .status(StatusCode::TOO_MANY_REQUESTS)
                .header("content-type", "application/json")
                .header("retry-after", "1")
                .body(axum::body::Body::from(body_rate_limited(turn)))
                .expect("valid HTTP response");
        }
    };

    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "text/event-stream")
        .header("cache-control", "no-cache")
        .body(axum::body::Body::from(sse_body))
        .expect("valid HTTP response")
}

// ─── Public API ───────────────────────────────────────────────────────────────

/// A running mock LLM server. Drop to shut down.
pub struct MockLlmServer {
    pub base_url: String,
    _shutdown: tokio::sync::oneshot::Sender<()>,
}

impl MockLlmServer {
    /// Start the mock server on a random free port. Returns immediately.
    pub async fn start(scenario: MockScenario) -> Result<Self, String> {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .map_err(|e| format!("mock server bind failed: {e}"))?;
        let addr: SocketAddr = listener.local_addr().expect("listener has local_addr");
        let base_url = format!("http://127.0.0.1:{}", addr.port());

        let state = ServerState {
            scenario,
            call_count: Arc::new(AtomicU32::new(0)),
        };

        let app = Router::new()
            .route("/chat/turn", post(handle_chat_turn))
            .with_state(state);

        let (tx, rx) = tokio::sync::oneshot::channel::<()>();

        tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async move {
                    let _ = rx.await;
                })
                .await
                .ok();
        });

        // Yield to let the server task start accepting connections
        tokio::task::yield_now().await;

        eprintln!(
            "  🧪 Mock LLM server: {} (scenario: {})",
            base_url,
            scenario.description()
        );

        Ok(Self {
            base_url,
            _shutdown: tx,
        })
    }
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // Test the SSE body generators directly (no HTTP server needed)

    #[test]
    fn complete_body_contains_required_events() {
        let body = body_complete("test-agent", 1);
        assert!(body.contains("session_info"));
        assert!(body.contains("text_delta"));
        assert!(body.contains("text_done"));
        assert!(body.contains("\"type\":\"done\""));
        assert!(body.contains("test-agent"));
    }

    #[test]
    fn tool_body_contains_tool_call_and_result() {
        let body = body_tool_then_complete("coder", 2);
        assert!(body.contains("tool_call_start"));
        assert!(body.contains("write_file"));
        assert!(body.contains("tool_result"));
        assert!(body.contains("text_done"));
        assert!(body.contains("\"type\":\"done\""));
    }

    #[test]
    fn fail_body_contains_error_event() {
        let body = body_fail("agent", 1);
        assert!(body.contains("\"type\":\"error\""));
        assert!(body.contains("mock_error"));
    }

    #[test]
    fn multi_turn_body_differs_by_turn() {
        let turn1 = body_multi_turn("agent", 1);
        let turn2 = body_multi_turn("agent", 2);
        assert!(turn1.contains("analyzing"));
        assert!(turn2.contains("finished on turn 2"));
        assert!(!turn1.contains("finished"));
    }

    #[test]
    fn all_sse_lines_are_valid_data_lines() {
        for body in [
            body_complete("a", 1),
            body_tool_then_complete("a", 1),
            body_fail("a", 1),
            body_multi_turn("a", 1),
            body_multi_turn("a", 2),
        ] {
            for chunk in body.split("\n\n").filter(|s| !s.is_empty()) {
                assert!(
                    chunk.starts_with("data: "),
                    "SSE chunk must start with 'data: ': {chunk:?}"
                );
                let json_str = &chunk["data: ".len()..];
                let parsed: serde_json::Value = serde_json::from_str(json_str)
                    .unwrap_or_else(|e| panic!("invalid JSON in SSE chunk: {e}\n{json_str}"));
                assert!(
                    parsed.get("type").is_some(),
                    "SSE event must have 'type' field: {parsed}"
                );
            }
        }
    }

    #[test]
    fn scenario_from_str_roundtrip() {
        for (name, _) in MockScenario::all() {
            assert!(
                MockScenario::from_str(name).is_some(),
                "scenario '{name}' not parseable"
            );
        }
        assert!(MockScenario::from_str("nonexistent").is_none());
        // Aliases
        assert_eq!(
            MockScenario::from_str("tool"),
            Some(MockScenario::ToolThenComplete)
        );
        assert_eq!(
            MockScenario::from_str("multi"),
            Some(MockScenario::MultiTurn)
        );
        assert_eq!(MockScenario::from_str("error"), Some(MockScenario::Fail));
        assert_eq!(
            MockScenario::from_str("chunk_split"),
            Some(MockScenario::SseChunkSplit)
        );
        assert_eq!(
            MockScenario::from_str("malformed"),
            Some(MockScenario::MalformedJson)
        );
        assert_eq!(
            MockScenario::from_str("429"),
            Some(MockScenario::RateLimited)
        );
        assert_eq!(
            MockScenario::from_str("no_tools"),
            Some(MockScenario::TextOnly)
        );
    }

    // ─── P1: adversarial scenario body tests ────────────────────────────────

    /// Parse raw SSE text into (event_id, data_payload) tuples exactly the
    /// way a compliant client would: `data:` lines within one event are
    /// joined with `\n`, blank line terminates the event.
    fn parse_sse_events(body: &str) -> Vec<String> {
        let mut events = Vec::new();
        let mut current = String::new();
        for line in body.split_inclusive('\n') {
            if line == "\n" || line == "\r\n" {
                if !current.is_empty() {
                    events.push(std::mem::take(&mut current));
                }
                continue;
            }
            let trimmed = line.trim_end_matches(['\r', '\n']);
            if let Some(rest) = trimmed.strip_prefix("data: ") {
                if !current.is_empty() {
                    current.push('\n');
                }
                current.push_str(rest);
            } else if let Some(rest) = trimmed.strip_prefix("data:") {
                if !current.is_empty() {
                    current.push('\n');
                }
                current.push_str(rest);
            }
        }
        if !current.is_empty() {
            events.push(current);
        }
        events
    }

    #[test]
    fn sse_chunk_split_event_reassembles_to_valid_tool_call_json() {
        let body = body_sse_chunk_split("coder", 1);
        let events = parse_sse_events(&body);

        // Find the event carrying the split tool_call — it must parse as
        // valid JSON ONLY after SSE reassembly.
        let tool_event = events
            .iter()
            .find(|e| e.contains("tool_call_start"))
            .expect("reassembled body must contain the tool_call event");

        let parsed: Value = serde_json::from_str(tool_event)
            .expect("after SSE reassembly the tool_call JSON must be valid");
        assert_eq!(parsed["type"], "tool_call_start");
        assert_eq!(parsed["call_id"], "call-split");
        assert_eq!(parsed["tool"]["name"], "write_file");

        // The raw body MUST actually contain a mid-JSON chunk boundary —
        // otherwise the scenario is a liar. Verify the split produces at
        // least one `data:` line whose standalone payload is NOT valid JSON.
        let raw_data_lines: Vec<&str> = body
            .lines()
            .filter_map(|l| l.strip_prefix("data: "))
            .collect();
        let tool_related: Vec<&&str> = raw_data_lines
            .iter()
            .filter(|l| l.contains("tool_call_start") || l.contains("\"arguments\":"))
            .collect();
        assert_eq!(
            tool_related.len(),
            2,
            "the tool_call event MUST span exactly 2 `data:` lines"
        );
        let at_least_one_half_is_invalid = tool_related
            .iter()
            .any(|l| serde_json::from_str::<Value>(l).is_err());
        assert!(
            at_least_one_half_is_invalid,
            "at least one half of the split event must be invalid JSON on \
             its own — otherwise the adversarial shape is not exercised"
        );
    }

    #[test]
    fn malformed_json_body_has_exactly_one_broken_frame_surrounded_by_valid_ones() {
        let body = body_malformed_json("agent", 7);
        let events = parse_sse_events(&body);

        let mut valid = 0usize;
        let mut invalid = 0usize;
        for e in &events {
            match serde_json::from_str::<Value>(e) {
                Ok(_) => valid += 1,
                Err(_) => invalid += 1,
            }
        }
        assert_eq!(
            invalid, 1,
            "exactly one event must be malformed JSON (got {invalid}): \
             events = {events:?}"
        );
        assert!(
            valid >= 2,
            "at least session_info + done must be valid (got valid={valid})"
        );
        // And the order must be: valid session_info FIRST, malformed in
        // the middle, valid done LAST — so a recovering parser still sees
        // turn start and turn end.
        let first_valid: Value = serde_json::from_str(&events[0]).unwrap();
        assert_eq!(first_valid["type"], "session_info");
        let last_valid: Value = serde_json::from_str(events.last().unwrap()).unwrap();
        assert_eq!(last_valid["type"], "done");
    }

    #[test]
    fn rate_limited_body_is_json_error_not_sse() {
        let body = body_rate_limited(3);
        // Must not be SSE — a non-200 response body should be structured
        // error JSON, not `data: ...` frames.
        assert!(
            !body.contains("data: "),
            "rate-limited body must not be SSE-framed, got: {body}"
        );
        let parsed: Value = serde_json::from_str(&body).expect("body must be JSON");
        assert_eq!(parsed["error"]["type"], "rate_limit_error");
        assert!(parsed["error"]["retry_after_seconds"].is_number());
    }

    #[test]
    fn text_only_body_emits_no_tool_call_events() {
        let body = body_text_only("assistant", 1);
        assert!(
            !body.contains("tool_call_start"),
            "text_only must not emit any tool_call_start events"
        );
        assert!(
            !body.contains("tool_result"),
            "text_only must not emit any tool_result events"
        );
        // Yet must still have a complete turn shape.
        let events = parse_sse_events(&body);
        let kinds: Vec<&str> = events
            .iter()
            .filter_map(|e| serde_json::from_str::<Value>(e).ok())
            .filter_map(|v| v["type"].as_str().map(str::to_string))
            .map(|s| Box::leak(s.into_boxed_str()) as &str)
            .collect();
        assert!(kinds.contains(&"session_info"));
        assert!(kinds.contains(&"text_done"));
        assert!(kinds.contains(&"done"));
    }

    #[test]
    fn all_new_scenarios_are_listed_and_descriptive() {
        let names: Vec<&str> = MockScenario::all().iter().map(|(n, _)| *n).collect();
        for s in [
            "sse_chunk_split",
            "malformed_json",
            "rate_limited",
            "text_only",
        ] {
            assert!(
                names.contains(&s),
                "new scenario '{s}' must be registered in MockScenario::all()"
            );
        }
    }
}
