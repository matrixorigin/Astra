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
}

impl MockScenario {
    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "complete" => Some(Self::Complete),
            "tool_then_complete" | "tool" => Some(Self::ToolThenComplete),
            "multi_turn" | "multi" => Some(Self::MultiTurn),
            "fail" | "error" => Some(Self::Fail),
            "slow" => Some(Self::Slow),
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
        }
    }

    pub fn all() -> &'static [(&'static str, &'static str)] {
        &[
            ("complete", "immediate completion (no tools)"),
            ("tool_then_complete", "one write_file tool call, then completion"),
            ("multi_turn", "two LLM turns: think then complete"),
            ("fail", "agent returns error"),
            ("slow", "3s delay before response (tests progress display)"),
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
    sse_line(&serde_json::json!({
        "type": "tool_call_start",
        "call_id": call_id,
        "tool": { "name": tool, "arguments": args.to_string() },
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
    s.push_str(&error_event("Mock agent failure: simulated LLM error for testing"));
    s
}

async fn body_slow(agent_id: &str, turn: u32) -> String {
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    body_complete(agent_id, turn)
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
        .and_then(|v| v.get("agent_id").and_then(Value::as_str).map(str::to_string))
        .unwrap_or_else(|| format!("agent-{turn}"));

    let sse_body = match state.scenario {
        MockScenario::Complete => body_complete(&agent_id, turn),
        MockScenario::ToolThenComplete => body_tool_then_complete(&agent_id, turn),
        MockScenario::MultiTurn => body_multi_turn(&agent_id, turn),
        MockScenario::Fail => body_fail(&agent_id, turn),
        MockScenario::Slow => body_slow(&agent_id, turn).await,
    };

    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "text/event-stream")
        .header("cache-control", "no-cache")
        .body(axum::body::Body::from(sse_body))
        .unwrap()
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
        let addr: SocketAddr = listener.local_addr().unwrap();
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
        assert_eq!(MockScenario::from_str("tool"), Some(MockScenario::ToolThenComplete));
        assert_eq!(MockScenario::from_str("multi"), Some(MockScenario::MultiTurn));
        assert_eq!(MockScenario::from_str("error"), Some(MockScenario::Fail));
    }
}
