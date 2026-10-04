//! Scripted HTTP/SSE fixtures for client protocol and UI tests without real LLM calls.
//!
//! Mock Server protocol facts for client tests; no Server execution or settlement is exercised.
//! Edge-tool fixtures validate exact callbacks on one stream. Inference-only
//! scenarios remain missing-terminal negative controls.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU8, AtomicU32, Ordering};

use axum::Router;
use axum::extract::State;
use axum::http::{HeaderMap, Method, StatusCode, Uri};
use axum::response::Response;
use axum::routing::{delete, get, post};
use serde_json::Value;
use tokio::net::TcpListener;

// ─── Scenario ────────────────────────────────────────────────────────────────

/// A named scenario that determines what SSE stream the mock server returns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MockScenario {
    /// Agent immediately outputs a completion message with canonical Server
    /// terminal evidence. No tool calls.
    Complete,
    /// Agent calls one edge tool (write_file), then completes.
    ToolThenComplete,
    /// Negative control: execute the callback, then close without Server terminal evidence.
    ToolThenMissingTerminal,
    /// Agent makes two LLM turns: first asks a question, second completes.
    MultiTurn,
    /// Agent returns an error response.
    Fail,
    /// Agent delays 3s before responding (tests timeout/progress display).
    Slow,
    /// Agent remains pending long enough for cancellation journeys to prove
    /// that Ctrl+C, rather than a naturally completed response, settles the turn.
    CancellationPending,
    /// Root activates and launches one asynchronous child, then acknowledges
    /// its launch receipt.
    AgentThenComplete,
    /// Server launches three slots; tests publish partial results and release
    /// the same root stream after observing the active run projection.
    FanoutThenComplete,
    /// The same Server fanout fixture, with a failed second slot.
    FanoutPartialThenComplete,
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
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "complete" => Some(Self::Complete),
            "tool_then_complete" | "tool" => Some(Self::ToolThenComplete),
            "tool_then_missing_terminal" => Some(Self::ToolThenMissingTerminal),
            "multi_turn" | "multi" => Some(Self::MultiTurn),
            "fail" | "error" => Some(Self::Fail),
            "slow" => Some(Self::Slow),
            "cancellation_pending" => Some(Self::CancellationPending),
            "agent_then_complete" | "agent" => Some(Self::AgentThenComplete),
            "fanout_then_complete" | "fanout" => Some(Self::FanoutThenComplete),
            "fanout_partial_then_complete" | "fanout_partial" => {
                Some(Self::FanoutPartialThenComplete)
            }
            "sse_chunk_split" | "sse_split" | "chunk_split" => Some(Self::SseChunkSplit),
            "malformed_json" | "malformed" | "bad_json" => Some(Self::MalformedJson),
            "rate_limited" | "rate_limit" | "429" => Some(Self::RateLimited),
            "text_only" | "inference_only" | "no_tools" => Some(Self::TextOnly),
            _ => None,
        }
    }

    pub fn description(self) -> &'static str {
        match self {
            Self::Complete => "immediate Server-owned completion (no tools)",
            Self::ToolThenComplete => "one write_file tool call, then completion",
            Self::ToolThenMissingTerminal => "callback executed but Server terminal missing",
            Self::MultiTurn => "two LLM turns: think then complete",
            Self::Fail => "agent returns error",
            Self::Slow => "3s delay before response (tests progress display)",
            Self::CancellationPending => "pending response for cancellation journeys",
            Self::AgentThenComplete => "one asynchronous child agent, then launch acknowledgement",
            Self::FanoutThenComplete => "Server-owned three-slot fanout with streamed completion",
            Self::FanoutPartialThenComplete => {
                "Server-owned three-slot fanout with one failed child"
            }
            Self::SseChunkSplit => "tool_call JSON split across SSE frames (adversarial)",
            Self::MalformedJson => "one SSE event carries malformed JSON (adversarial)",
            Self::RateLimited => "HTTP 429 with Retry-After (adversarial)",
            Self::TextOnly => "text completion only, no tool_calls (inference-only)",
        }
    }

    pub fn all() -> &'static [(&'static str, &'static str)] {
        &[
            ("complete", "immediate Server-owned completion (no tools)"),
            (
                "tool_then_complete",
                "one write_file tool call, then completion",
            ),
            (
                "tool_then_missing_terminal",
                "callback executed but Server terminal missing",
            ),
            ("multi_turn", "two LLM turns: think then complete"),
            ("fail", "agent returns error"),
            ("slow", "3s delay before response (tests progress display)"),
            (
                "cancellation_pending",
                "pending response for cancellation journeys",
            ),
            (
                "agent_then_complete",
                "one asynchronous child agent, then launch acknowledgement",
            ),
            (
                "fanout_then_complete",
                "Server-owned three-slot fanout with streamed completion",
            ),
            (
                "fanout_partial_then_complete",
                "Server-owned three-slot fanout with one failed child",
            ),
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
    // `tool: {name, arguments}` is rejected as a producer contract violation.
    // The regression anchor is
    // `phase_r2_mock_dispatch_contract::mock_llm_tool_call_start_shape_is_captured_by_dispatch`.
    sse_line(&serde_json::json!({
        "type": "tool_call_start",
        "call_id": call_id,
        "tool": tool,
        "arguments": args.to_string(),
    }))
}

fn tool_request(call_id: &str, tool: &str, args: Value) -> String {
    tool_request_for_run(call_id, tool, args, "mock-run-tool", "mock-turn-chain-tool")
}

fn tool_request_for_run(
    call_id: &str,
    tool: &str,
    args: Value,
    run_id: &str,
    turn_chain_id: &str,
) -> String {
    sse_line(&serde_json::json!({
        "type": "tool_request",
        "session_id": "mock-session",
        "run_id": run_id,
        "turn_chain_id": turn_chain_id,
        "request_id": call_id,
        "schema_admitted_by_server": true,
        "execution_timeout_ms": 300_000, "command_timeout_cap_ms": 30_000,
        "execution_deadline_unix_ms": 4_102_444_800_000_u64,
        "tool": tool,
        "args": args,
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
    // Emit a FLAT `usage` event first (matching the real server at
    // server_loop_host.rs line 822 and ws_handler.rs line 2505), THEN
    // the terminal `done` event. Dispatch has no handler for `done` —
    // tokens ride on the `usage` event only.
    //
    // Regression anchor: phase_r2_mock_dispatch_contract::
    //   mock_llm_terminal_sequence_populates_usage_tokens
    //   mock_done_event_alone_leaves_usage_unset_regression_anchor
    let usage = sse_line(&serde_json::json!({
        "type": "usage",
        "input_tokens": tokens,
        "cached_input_tokens": 0u64,
        "cache_creation_tokens": 0u64,
        "output_tokens": 50u64,
        "total_tokens": tokens + 50,
    }));
    let done = sse_line(&serde_json::json!({
        "type": "done",
        "tokens_used": tokens,
        "usage": {
            "input_tokens": tokens,
            "cached_input_tokens": 0u64,
            "cache_creation_tokens": 0u64,
            "output_tokens": 50u64,
            "total_tokens": tokens + 50,
        },
    }));
    format!("{usage}{done}")
}

/// Emit canonical completion and usage for the fixture's Server-owned run.
/// Inference-only TextOnly responses deliberately omit this boundary.
fn server_terminal_event(run_id: &str, assistant_text: &str, tokens: u64) -> String {
    let receipt = astra_turn_core::tool_ledger_receipt::ToolLedgerReceipt::empty(run_id, 1);
    let finished = sse_line(&serde_json::json!({
        "type": "run_finished",
        "run_id": run_id,
        "status": "completed",
        "owner_generation": 1
    }));
    let terminal = sse_line(&serde_json::json!({
        "type": "turn_complete",
        "has_tool_calls": false,
        "continuation_owner": "server",
        "assistant_text": assistant_text,
        "tool_calls_count": 0,
        "observation_tool_calls_count": 0,
        "tools_used": [],
        "llm_rounds": 1,
        "tool_ledger_receipt": receipt,
        "token_usage_coverage": {
            "scope": "logical_provider_calls",
            "attempts": 1,
            "provider_reported": 1,
            "unavailable": 0,
            "status": "complete"
        }
    }));
    format!("{finished}{terminal}{}", done_event(tokens))
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
    let run_id = format!("mock-run-{turn}");
    let msg = format!(
        "Task completed by {agent_id} (turn {turn}). \
         I have finished the assigned work successfully."
    );
    let mut s = String::new();
    s.push_str(&session_info(&run_id));
    s.push_str(&text_delta(&msg));
    s.push_str(&text_done(&msg));
    s.push_str(&server_terminal_event(&run_id, &msg, 200));
    s
}

fn body_tool_then_complete(agent_id: &str, turn: u32) -> String {
    // These are consecutive Server-owned provider rounds in one HTTP stream.
    if turn == 1 {
        let path = format!("mock-output-{agent_id}.txt");
        let content = format!("Output from {agent_id}");
        let args = serde_json::json!({ "path": path, "content": content });
        let mut s = session_info("mock-run-tool");
        s.push_str(&tool_call_start("call-1", "write_file", args.clone()));
        s.push_str(&tool_request("call-1", "write_file", args));
        s.push_str(&done_event(150));
        return s;
    }

    let msg = format!("{agent_id}: wrote the requested file and completed task.");
    let mut s = session_info("mock-run-tool");
    s.push_str(&text_delta(&msg));
    s.push_str(&text_done(&msg));
    s.push_str(&server_terminal_event("mock-run-tool", &msg, 200));
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

async fn body_slow(
    agent_id: &str,
    turn: u32,
    held_response_release: Option<&tokio::sync::Notify>,
) -> String {
    match held_response_release {
        Some(release) => release.notified().await,
        None => tokio::time::sleep(std::time::Duration::from_secs(3)).await,
    }
    body_complete(agent_id, turn)
}

async fn body_cancellation_pending(agent_id: &str, turn: u32) -> String {
    // This is deliberately longer than the journey's own settlement timeout.
    // A passing cancellation journey must therefore be released by Ctrl+C and
    // cannot accidentally pass or fail because a short mock delay elapsed.
    tokio::time::sleep(std::time::Duration::from_secs(60)).await;
    body_complete(agent_id, turn)
}

fn server_message(body: &Value) -> Option<&str> {
    body.get("message").and_then(Value::as_str)
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

/// Inference-only: text completion with NO tool_call_start or Server-owned
/// terminal events. Even if a caller passed tool schemas in the request, the
/// model is free to answer with text, but this response must not be admitted
/// as a successful Server-owned execution.
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

#[derive(Clone, Debug, PartialEq, Eq)]
struct IssuedToolRequestIdentity {
    session_id: String,
    run_id: String,
    turn_chain_id: String,
    request_id: String,
    tool: String,
    args: Value,
}

impl IssuedToolRequestIdentity {
    fn from_sse_event(event: &Value) -> Option<Self> {
        if event.get("type").and_then(Value::as_str) != Some("tool_request")
            || event
                .get("schema_admitted_by_server")
                .and_then(Value::as_bool)
                != Some(true)
        {
            return None;
        }

        let identity = Self {
            session_id: event.get("session_id")?.as_str()?.to_string(),
            run_id: event.get("run_id")?.as_str()?.to_string(),
            turn_chain_id: event.get("turn_chain_id")?.as_str()?.to_string(),
            request_id: event.get("request_id")?.as_str()?.to_string(),
            tool: event.get("tool")?.as_str()?.to_owned(),
            args: event.get("args")?.clone(),
        };
        (!identity.session_id.trim().is_empty()
            && !identity.run_id.trim().is_empty()
            && !identity.turn_chain_id.trim().is_empty()
            && !identity.request_id.trim().is_empty())
        .then_some(identity)
    }

    fn matches_result(&self, result: &astra_thin_client::ToolResultRequest) -> bool {
        self.session_id == result.session_id
            && self.run_id == result.run_id
            && self.turn_chain_id == result.turn_chain_id
            && self.request_id == result.request_id
    }
}

fn issued_tool_requests_from_sse(body: &str) -> Vec<IssuedToolRequestIdentity> {
    body.split("\n\n")
        .filter_map(|event_block| {
            let data = event_block
                .lines()
                .filter_map(|line| line.strip_prefix("data: "))
                .collect::<Vec<_>>()
                .join("\n");
            (!data.is_empty())
                .then(|| serde_json::from_str::<Value>(&data).ok())
                .flatten()
        })
        .filter_map(|event| IssuedToolRequestIdentity::from_sse_event(&event))
        .collect()
}

fn record_issued_tool_requests(ledger: &Arc<Mutex<Vec<IssuedToolRequestIdentity>>>, body: &str) {
    let issued = issued_tool_requests_from_sse(body);
    if issued.is_empty() {
        return;
    }
    if let Ok(mut ledger) = ledger.lock() {
        const MAX_ISSUED_TOOL_REQUESTS: usize = 64;
        for identity in issued {
            if !ledger.contains(&identity) {
                if ledger.len() == MAX_ISSUED_TOOL_REQUESTS {
                    ledger.remove(0);
                }
                ledger.push(identity);
            }
        }
    }
}

#[derive(Clone)]
struct ServerState {
    scenario: MockScenario,
    call_count: Arc<AtomicU32>,
    received_requests: Arc<Mutex<Vec<Value>>>,
    issued_tool_requests: Arc<Mutex<Vec<IssuedToolRequestIdentity>>>,
    tool_results: Arc<Mutex<Vec<Value>>>,
    completed_fanout_children: Arc<AtomicU8>,
    cancelled_runs: Arc<Mutex<Vec<String>>>,
    callback_ready: Arc<tokio::sync::Notify>,
    emitted_text: Arc<Mutex<Vec<(String, String)>>>,
    held_response_release: Option<Arc<tokio::sync::Notify>>,
}

/// Keep tool dispatch and continuation under one Server-owned response. Dropping
/// the HTTP body drops callback waiting too; there is no detached continuation.
fn orchestration_response(state: ServerState, request: Value) -> Response<axum::body::Body> {
    let stream = futures_util::stream::unfold(
        (
            state,
            request,
            Vec::<IssuedToolRequestIdentity>::new(),
            false,
            0u32,
            Vec::<IssuedToolRequestIdentity>::new(),
            0u64,
            0u64,
        ),
        |(
            state,
            request,
            pending,
            finished,
            round,
            mut all_issued,
            mut input_total,
            mut output_total,
        )| async move {
            if finished {
                return None;
            }
            for identity in &pending {
                let wait = async {
                    loop {
                        let notified = state.callback_ready.notified();
                        tokio::pin!(notified);
                        notified.as_mut().enable();
                        let status = state.tool_results.lock().ok().and_then(|records| {
                            records
                                .iter()
                                .find(|record| {
                                    record.get("request_id").and_then(Value::as_str)
                                        == Some(identity.request_id.as_str())
                                        && record.get("run_id").and_then(Value::as_str)
                                            == Some(identity.run_id.as_str())
                                        && record.get("session_id").and_then(Value::as_str)
                                            == Some(identity.session_id.as_str())
                                        && record.get("turn_chain_id").and_then(Value::as_str)
                                            == Some(identity.turn_chain_id.as_str())
                                })
                                .and_then(|record| record.get("status").and_then(Value::as_str))
                                .map(str::to_owned)
                        });
                        if let Some(status) = status {
                            return status == "completed";
                        }
                        notified.await;
                    }
                };
                if tokio::time::timeout(std::time::Duration::from_secs(30), wait).await != Ok(true)
                {
                    let chunk = error_event("tool callback failed or timed out");
                    return Some((
                        Ok::<_, std::io::Error>(chunk),
                        (
                            state,
                            request,
                            pending,
                            true,
                            round,
                            all_issued,
                            input_total,
                            output_total,
                        ),
                    ));
                }
            }
            let body = match state.scenario {
                MockScenario::ToolThenMissingTerminal if round > 0 => String::new(),
                MockScenario::ToolThenComplete | MockScenario::ToolThenMissingTerminal => {
                    body_tool_then_complete(
                        request
                            .get("agent_id")
                            .and_then(Value::as_str)
                            .unwrap_or("mock-agent"),
                        round + 1,
                    )
                }
                _ => unreachable!("only Edge-tool scenarios wait for callbacks"),
            };
            let run_id = body
                .lines()
                .filter_map(|line| line.strip_prefix("data: "))
                .filter_map(|line| serde_json::from_str::<Value>(line).ok())
                .find(|event| event["type"] == "session_info")
                .and_then(|event| event["run_id"].as_str().map(str::to_owned));
            let issued = issued_tool_requests_from_sse(&body);
            record_issued_tool_requests(&state.issued_tool_requests, &body);
            let finished = issued.is_empty();
            all_issued.extend(issued.iter().cloned());
            let mut chunk = String::new();
            for line in body.lines().filter_map(|line| line.strip_prefix("data: ")) {
                let Ok(mut event) = serde_json::from_str::<Value>(line) else {
                    continue;
                };
                if event["type"] == "usage" {
                    input_total += event["input_tokens"].as_u64().expect("fixture usage");
                    output_total += event["output_tokens"].as_u64().expect("fixture usage");
                }
                match event.get("type").and_then(Value::as_str) {
                    Some("done") if !finished => continue,
                    Some("session_info") if round > 0 => continue,
                    Some("turn_complete") if round > 0 => {
                        use astra_turn_core::tool_ledger_receipt::{
                            EMPTY_TOOL_LEDGER_ROOT, ToolLedgerReceipt, ToolLedgerResultClassCounts,
                            roll_tool_ledger_root,
                        };
                        let run_id = event["tool_ledger_receipt"]["run_id"]
                            .as_str()
                            .expect("fixture terminal run");
                        let records = state.tool_results.lock().expect("fixture callback ledger");
                        let records: Vec<_> = records
                            .iter()
                            .filter(|record| {
                                all_issued.iter().any(|identity| {
                                    record["run_id"].as_str() == Some(identity.run_id.as_str())
                                        && record["session_id"].as_str()
                                            == Some(identity.session_id.as_str())
                                        && record["turn_chain_id"].as_str()
                                            == Some(identity.turn_chain_id.as_str())
                                        && record["request_id"].as_str()
                                            == Some(identity.request_id.as_str())
                                })
                            })
                            .collect();
                        let mut root = EMPTY_TOOL_LEDGER_ROOT.to_owned();
                        for (index, record) in records.iter().enumerate() {
                            root = roll_tool_ledger_root(
                                &root,
                                index as u64 + 1,
                                record["request_id"]
                                    .as_str()
                                    .expect("validated callback id"),
                                "succeeded",
                            );
                        }
                        let count = records.len() as u32;
                        event["tool_ledger_receipt"] =
                            serde_json::to_value(ToolLedgerReceipt::new(
                                run_id,
                                1,
                                count,
                                count,
                                0,
                                ToolLedgerResultClassCounts {
                                    succeeded: count,
                                    ..Default::default()
                                },
                                u64::from(count),
                                root,
                                true,
                            ))
                            .expect("receipt serializes");
                        event["tool_calls_count"] = count.into();
                        event["observation_tool_calls_count"] = 0.into();
                        event["tools_used"] = serde_json::json!(["write_file"]);
                        event["llm_rounds"] = (round + 1).into();
                        event["token_usage_coverage"]["attempts"] = (round + 1).into();
                        event["token_usage_coverage"]["provider_reported"] = (round + 1).into();
                    }
                    Some("usage") => {
                        let last_input =
                            event["input_tokens"].as_u64().expect("fixture input usage");
                        let input = input_total;
                        let output = output_total;
                        event["input_tokens"] = input.into();
                        event["output_tokens"] = output.into();
                        event["total_tokens"] = (input + output).into();
                        event["usage_scope"] = "run_total".into();
                        event["qualified_usage"] = serde_json::to_value(
                            astra_turn_types::CanonicalTokenUsage::new(
                                Some(input),
                                Some(0),
                                Some(0),
                                Some(output),
                            )
                            .expect("fixture usage"),
                        )
                        .expect("usage serializes");
                        event["last_request_usage"] = serde_json::json!({"prompt_tokens": last_input, "cache_read_tokens": 0, "cache_creation_tokens": 0, "completion_tokens": 50, "input_total_tokens": last_input});
                    }
                    _ => {}
                }
                if event["type"] == "text_done"
                    && let Some(run_id) = &run_id
                    && let Some(text) = event["full_text"].as_str()
                {
                    let mut emitted = state.emitted_text.lock().expect("fixture emitted text");
                    if emitted.len() == 32 {
                        emitted.remove(0);
                    }
                    emitted.push((run_id.clone(), text.to_owned()));
                }
                chunk.push_str(&sse_line(&event));
            }
            Some((
                Ok::<_, std::io::Error>(chunk),
                (
                    state,
                    request,
                    issued,
                    finished,
                    round + 1,
                    all_issued,
                    input_total,
                    output_total,
                ),
            ))
        },
    );
    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "text/event-stream")
        .header(
            astra_server_types::AGENT_INTERACTION_API_MAJOR_HEADER,
            astra_server_types::AGENT_INTERACTION_API_MAJOR,
        )
        .header("cache-control", "no-cache")
        .body(axum::body::Body::from_stream(stream))
        .expect("valid HTTP response")
}

/// Preset Server-owned control-plane execution. No agent tool is dispatched to
/// the client, and no child admission or client reconciliation is needed.
fn server_orchestration_response(state: ServerState) -> Response<axum::body::Body> {
    let stream = futures_util::stream::unfold((state, 0u8), |(state, phase)| async move {
        if phase == 2 {
            return None;
        }
        let snapshot = orchestration_snapshot(&state).expect("orchestration scenario");
        let single = state.scenario == MockScenario::AgentThenComplete;
        let root = if single {
            "mock-run-agent-root"
        } else {
            "mock-run-fanout-root"
        };
        let tool = if single { "agent" } else { "agent_fanout" };
        let call = "mock-control-call";
        let mut body = String::new();
        if phase == 0 {
            body.push_str(&session_info(root));
            let agents = snapshot.runs.iter().enumerate().map(|(index, run)| serde_json::json!({
                "slot_index": index, "id": format!("review-{}", index+1),
                "requested_description": run.agent_name, "agent_id": run.agent_id,
                "run_id": run.run_id, "status": "launched", "transcript_location": "durable_server"
            })).collect::<Vec<_>>();
            let receipt = if single {
                serde_json::json!({
                    "status": "launched", "agent_id": snapshot.runs[0].agent_id,
                    "run_id": snapshot.runs[0].run_id, "description": "Mock child review",
                    "parent_run_id": root, "transcript_location": "durable_server"
                })
            } else {
                serde_json::json!({
                    "status": "started", "group_id": "mock-review-group", "title": "Three mock reviews",
                    "target_count": 3, "transcript_location": "durable_server",
                    "fanout": {"parent_run_id": root}, "agents": agents
                })
            };
            let arguments = if single {
                serde_json::json!({"action":"spawn", "description":"Mock child review", "prompt":"Review the assigned work and report findings."})
            } else {
                serde_json::json!({"action":"start", "group_id":"mock-review-group", "title":"Three mock reviews", "target_count":snapshot.runs.len(),
                    "slots": snapshot.runs.iter().enumerate().map(|(index, run)| serde_json::json!({
                        "id":format!("review-{}",index+1), "description":run.agent_name, "prompt":"Review the assigned work and report findings."
                    })).collect::<Vec<_>>()})
            };
            body.push_str(&sse_line(&serde_json::json!({
                "type": "tool_call_start", "call_id": call, "tool": tool,
                "arguments": arguments.to_string(),
                "transport": "server_local", "run_id": root
            })));
            body.push_str(&sse_line(&serde_json::json!({
                "type": "tool_call_end", "call_id": call, "tool": tool,
                "status": "ok", "duration_ms": 1, "result": receipt,
                "transport": "server_local", "run_id": root
            })));
            for run in &snapshot.runs {
                body.push_str(&sse_line(&serde_json::json!({
                    "type": "agent_live_event", "run_id": run.run_id, "agent_id": run.agent_id,
                    "event_kind": "signal", "signal": {"signal": "run_started",
                        "parent_run_id": root, "depth": 1, "spawn_tool_call_id": null,
                        "transcript_location": "durable_server"}
                })));
            }
            body.push_str(&text_delta(if single {
                "Parent acknowledged the child launch."
            } else {
                "Three mock reviews are running."
            }));
        } else {
            match &state.held_response_release {
                Some(release) => release.notified().await,
                None => tokio::time::sleep(std::time::Duration::from_secs(3)).await,
            }
            state
                .completed_fanout_children
                .store(if single { 1 } else { 7 }, Ordering::Release);
            let snapshot = orchestration_snapshot(&state).expect("orchestration scenario");
            for run in &snapshot.runs {
                body.push_str(&sse_line(&serde_json::json!({
                    "type": "agent_live_event", "run_id": run.run_id, "agent_id": run.agent_id,
                    "event_kind": "agent_terminated", "termination": run.status,
                    "duration_ms": 1, "reason": run.error_message
                })));
            }
            let message =
                if snapshot.runs.iter().any(|run| {
                    run.status == astra_thin_client::SessionRunLifecycleStatus::Cancelled
                }) {
                    "Parent collected the settled child results, including cancellation."
                } else if single {
                    "Parent acknowledged the child launch."
                } else if state.scenario == MockScenario::FanoutPartialThenComplete {
                    "Parent reconciled 2 completed and 1 failed slot."
                } else {
                    "Parent reconciled one terminal fanout group."
                };
            body.push_str(&text_delta(message));
            body.push_str(&text_done(message));
            // This receipt describes the single Server control call emitted
            // above. It does not claim that the fixture tested Run settlement.
            use astra_turn_core::tool_ledger_receipt::{
                EMPTY_TOOL_LEDGER_ROOT, ToolLedgerReceipt, ToolLedgerResultClassCounts,
                roll_tool_ledger_root,
            };
            let receipt = ToolLedgerReceipt::new(
                root,
                1,
                1,
                1,
                0,
                ToolLedgerResultClassCounts {
                    succeeded: 1,
                    ..Default::default()
                },
                1,
                roll_tool_ledger_root(EMPTY_TOOL_LEDGER_ROOT, 1, call, "succeeded"),
                true,
            );
            for line in server_terminal_event(root, message, 140)
                .lines()
                .filter_map(|line| line.strip_prefix("data: "))
            {
                let mut event: Value = serde_json::from_str(line).expect("fixture event");
                if event["type"] == "turn_complete" {
                    event["tool_calls_count"] = 1.into();
                    event["tools_used"] = serde_json::json!([tool]);
                    event["tool_ledger_receipt"] = serde_json::to_value(&receipt).unwrap();
                }
                body.push_str(&sse_line(&event));
            }
        }
        for event in body
            .lines()
            .filter_map(|line| line.strip_prefix("data: "))
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        {
            let text = match event["type"].as_str() {
                Some("text_delta") => event["delta"]
                    .as_str()
                    .or_else(|| event["content"].as_str()),
                Some("text_done") => event["full_text"].as_str(),
                _ => None,
            };
            if let Some(text) = text {
                let mut recorded = state.emitted_text.lock().expect("fixture transcript");
                if recorded.last().is_none_or(|(_, previous)| previous != text) {
                    recorded.push((root.to_owned(), text.to_owned()));
                }
            }
        }
        Some((Ok::<_, std::io::Error>(body), (state, phase + 1)))
    });
    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "text/event-stream")
        .header(
            astra_server_types::AGENT_INTERACTION_API_MAJOR_HEADER,
            astra_server_types::AGENT_INTERACTION_API_MAJOR,
        )
        .body(axum::body::Body::from_stream(stream))
        .expect("fixture stream")
}

async fn handle_chat_turn(
    State(state): State<ServerState>,
    body: axum::body::Bytes,
) -> Response<axum::body::Body> {
    let turn = state.call_count.fetch_add(1, Ordering::Relaxed) + 1;
    let request_body = serde_json::from_slice::<Value>(&body).unwrap_or(Value::Null);
    if let Ok(mut requests) = state.received_requests.lock() {
        const MAX_RECORDED_REQUESTS: usize = 32;
        if requests.len() == MAX_RECORDED_REQUESTS {
            requests.remove(0);
        }
        requests.push(request_body.clone());
    }

    // Extract agent_id from top-level payload field (set by chat_turn_base_payload)
    let agent_id = request_body
        .get("agent_id")
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| format!("agent-{turn}"));

    if matches!(
        state.scenario,
        MockScenario::AgentThenComplete
            | MockScenario::FanoutThenComplete
            | MockScenario::FanoutPartialThenComplete
    ) {
        return server_orchestration_response(state);
    }

    if matches!(
        state.scenario,
        MockScenario::ToolThenComplete | MockScenario::ToolThenMissingTerminal
    ) {
        return orchestration_response(state, request_body);
    }

    let sse_body = match state.scenario {
        MockScenario::AgentThenComplete
        | MockScenario::FanoutThenComplete
        | MockScenario::FanoutPartialThenComplete => {
            unreachable!("Server agent scenarios own their stream")
        }
        MockScenario::Complete => body_complete(&agent_id, turn),
        MockScenario::ToolThenComplete | MockScenario::ToolThenMissingTerminal => {
            unreachable!("tool scenarios use one orchestration stream")
        }
        MockScenario::MultiTurn => body_multi_turn(&agent_id, turn),
        MockScenario::Fail => body_fail(&agent_id, turn),
        MockScenario::Slow => {
            body_slow(&agent_id, turn, state.held_response_release.as_deref()).await
        }
        MockScenario::CancellationPending => body_cancellation_pending(&agent_id, turn).await,
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
    // Callback authority is the exact request identity this mock emitted,
    // rather than a second scenario-name allowlist that can drift from the
    // scripted SSE protocol. Record it before returning the response so a
    // fast client cannot race the mock's ledger projection.
    record_issued_tool_requests(&state.issued_tool_requests, &sse_body);

    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "text/event-stream")
        .header(
            astra_server_types::AGENT_INTERACTION_API_MAJOR_HEADER,
            astra_server_types::AGENT_INTERACTION_API_MAJOR,
        )
        .header("cache-control", "no-cache")
        .body(axum::body::Body::from(sse_body))
        .expect("valid HTTP response")
}

async fn handle_unimplemented_mock_route(method: Method, uri: Uri) -> StatusCode {
    eprintln!("  mock LLM server has no route for {method} {uri}");
    StatusCode::NOT_FOUND
}

fn mock_model_catalog_entry(name: &str) -> Value {
    serde_json::json!({
        "offering_id": format!("offer-{name}"),
        "access_id": "self-hosted",
        "access_kind": "self_hosted",
        "access_label": "Self-hosted",
        "execution_placement": "server",
        "name": name,
        "provider": "mock",
        "description": null,
        "is_active": true,
        "context_window": 200_000,
        "max_completion_tokens": null,
        "architecture": null,
        "thinking_capability": null
    })
}

async fn handle_models() -> axum::Json<Value> {
    let items = mock_model_catalog();
    axum::Json(serde_json::json!({
        "items": items,
        "next_cursor": null,
        "limit": 50,
        "total": 3,
        "catalog_revision": "sha256:mock-catalog"
    }))
}

fn mock_model_catalog() -> Vec<Value> {
    ["gpt-5", "test-model", "mock-model"]
        .into_iter()
        .map(mock_model_catalog_entry)
        .collect()
}

async fn handle_model_access() -> axum::Json<Value> {
    let offerings = mock_model_catalog();
    axum::Json(serde_json::json!({
        "accesses": [{
            "id": "self-hosted",
            "kind": "self_hosted",
            "label": "Self-hosted",
            "execution_placement": "server",
            "status": "ready",
            "reason": null,
            "usable": true,
            "retry_after_seconds": null,
            "available_model_count": offerings.len(),
            "actions": []
        }],
        "default_offering_id": "offer-gpt-5",
        "next_cursor": null,
        "limit": 50,
        "total": 3,
        "catalog_revision": "sha256:mock-catalog",
        "observed_at": "2026-07-20T00:00:00Z",
        "offerings": offerings
    }))
}

async fn handle_model_admission(
    axum::Json(request): axum::Json<astra_server_types::ModelAdmissionRequestV1>,
) -> Result<axum::Json<Value>, (StatusCode, String)> {
    let catalog = mock_model_catalog();
    let mut slots = Vec::with_capacity(request.slots.len());
    for requested in request.slots {
        let model = catalog
            .iter()
            .find(|model| match &requested.selector {
                astra_turn_types::ModelSelector::OfferingId { offering_id } => {
                    model["offering_id"].as_str() == Some(offering_id.as_str())
                }
                astra_turn_types::ModelSelector::ConfiguredName { model_name, .. } => model["name"]
                    .as_str()
                    .is_some_and(|name| name.eq_ignore_ascii_case(model_name)),
            })
            .ok_or_else(|| (StatusCode::BAD_REQUEST, "mock Offering unavailable".into()))?;
        slots.push(serde_json::json!({
            "offering_id": model["offering_id"],
            "model_name": model["name"],
            "context_window": model["context_window"],
            "max_output_tokens": requested.max_output_tokens,
            "reasoning": requested.inherited_reasoning.as_ref()
                .filter(|inherited| model["offering_id"].as_str() == Some(inherited.offering_id.as_str()))
                .map(|inherited| &inherited.reasoning).unwrap_or(&requested.reasoning),
        }));
    }
    Ok(axum::Json(serde_json::json!({"slots": slots})))
}

async fn handle_create_session() -> axum::Json<Value> {
    axum::Json(serde_json::json!({ "session_id": "mock-session" }))
}

async fn handle_tool_result(
    State(state): State<ServerState>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Result<axum::Json<Value>, (StatusCode, axum::Json<Value>)> {
    let record =
        serde_json::from_slice::<astra_thin_client::ToolResultRequest>(&body).map_err(|error| {
            (
                StatusCode::BAD_REQUEST,
                axum::Json(serde_json::json!({
                    "accepted": false,
                    "error": format!("invalid tool result envelope: {error}"),
                })),
            )
        })?;

    let has_valid_identity = state
        .issued_tool_requests
        .lock()
        .map(|issued| {
            issued
                .iter()
                .any(|identity| identity.matches_result(&record))
        })
        .unwrap_or(false)
        && !record.edge_agent_id.trim().is_empty()
        && matches!(record.status.as_str(), "completed" | "failed" | "skipped")
        && record.result_hash
            == astra_thin_client::ToolResultRequest::compute_result_hash(
                astra_thin_client::ToolResultHashParts {
                    session_id: &record.session_id,
                    run_id: &record.run_id,
                    turn_chain_id: &record.turn_chain_id,
                    request_id: &record.request_id,
                    edge_agent_id: &record.edge_agent_id,
                    status: &record.status,
                    output: &record.output,
                    duration_ms: record.duration_ms,
                    tool_result_fields: record.tool_result_fields.as_ref(),
                },
            );
    let header_edge_id = headers
        .get(astra_thin_client::ASTRA_EDGE_ID_HEADER)
        .and_then(|value| value.to_str().ok());
    if !has_valid_identity || header_edge_id != Some(record.edge_agent_id.as_str()) {
        return Err((
            StatusCode::BAD_REQUEST,
            axum::Json(serde_json::json!({
                "accepted": false,
                "error": "tool result identity, status, hash, or edge header is invalid",
            })),
        ));
    }

    if let Ok(mut records) = state.tool_results.lock() {
        const MAX_RECORDED_TOOL_RESULTS: usize = 64;
        let value = serde_json::to_value(&record).expect("tool result request serializes");
        if let Some(previous) = records.iter().find(|previous| {
            previous["request_id"] == value["request_id"] && previous["run_id"] == value["run_id"]
        }) {
            if previous != &value {
                return Err((
                    StatusCode::CONFLICT,
                    axum::Json(
                        serde_json::json!({"accepted": false, "error": "conflicting callback retry"}),
                    ),
                ));
            }
        } else {
            if records.len() == MAX_RECORDED_TOOL_RESULTS {
                records.remove(0);
            }
            records.push(value);
        }
    }
    state.callback_ready.notify_waiters();
    Ok(axum::Json(serde_json::json!({"accepted": true})))
}

// Fixed Server facts for the orchestration journeys. These are protocol
// fixtures, not a second child executor or an implementation of Run settlement.
fn orchestration_snapshot(
    state: &ServerState,
) -> Result<astra_thin_client::SessionRunTreeSnapshot, StatusCode> {
    use astra_thin_client::{
        SESSION_RUN_TREE_SCHEMA_VERSION, SessionRunAction, SessionRunLifecycleStatus as Status,
        SessionRunNode, SessionRunRuntimeFacts, SessionRunTreeSnapshot,
    };
    let (root, count) = match state.scenario {
        MockScenario::AgentThenComplete => ("mock-run-agent-root", 1),
        MockScenario::FanoutThenComplete | MockScenario::FanoutPartialThenComplete => {
            ("mock-run-fanout-root", 3)
        }
        _ => return Err(StatusCode::NOT_FOUND),
    };
    let admitted = !state
        .received_requests
        .lock()
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .is_empty();
    let completed = state.completed_fanout_children.load(Ordering::Acquire);
    let cancelled = state
        .cancelled_runs
        .lock()
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let mut runs = Vec::new();
    if admitted {
        for index in 0..count {
            let run_id = if count == 1 {
                "mock-run-agent-child".to_owned()
            } else {
                format!("mock-run-fanout-child-{index}")
            };
            let status = if cancelled.contains(&run_id) {
                Status::Cancelled
            } else if completed & (1 << index) == 0 {
                Status::Running
            } else if state.scenario == MockScenario::FanoutPartialThenComplete && index == 1 {
                Status::Failed
            } else {
                Status::Completed
            };
            runs.push(SessionRunNode {
                run_id,
                parent_run_id: Some(root.into()),
                root_run_id: Some(root.into()),
                depth: 1,
                agent_id: Some(format!("mock-review-{}", index + 1)),
                agent_name: Some(if count == 1 {
                    "Mock child review".into()
                } else {
                    format!("Mock review {}", index + 1)
                }),
                status,
                waiting_for: None,
                error_code: (status == Status::Failed).then(|| "mock_review_failed".into()),
                error_message: (status == Status::Failed)
                    .then(|| "fanout_child_2_failed_with_distinct_cause".into()),
                run_event_high_watermark: if status.is_terminal() { 2 } else { 1 },
                total_tool_calls: 0,
                runtime: SessionRunRuntimeFacts {
                    background: Some(true),
                    ..Default::default()
                },
                available_actions: if status.is_terminal() {
                    vec![]
                } else {
                    vec![SessionRunAction::Cancel]
                },
                created_at: "2026-10-04T00:00:00Z".into(),
                updated_at: "2026-10-04T00:00:01Z".into(),
            });
        }
    }
    Ok(SessionRunTreeSnapshot {
        schema_version: SESSION_RUN_TREE_SCHEMA_VERSION,
        session_id: "mock-session".into(),
        snapshot_revision: format!("mock:{admitted}:{completed}:{}", cancelled.join(",")),
        observed_at: "2026-10-04T00:00:01Z".into(),
        node_limit: 200,
        truncated: false,
        runs,
    })
}

async fn handle_run_tree(
    State(state): State<ServerState>,
    axum::extract::Path(session_id): axum::extract::Path<String>,
) -> Result<axum::Json<astra_thin_client::SessionRunTreeSnapshot>, StatusCode> {
    if session_id != "mock-session" {
        return Err(StatusCode::NOT_FOUND);
    }
    orchestration_snapshot(&state).map(axum::Json)
}

async fn handle_cancel_run(
    State(state): State<ServerState>,
    axum::extract::Path(run_id): axum::extract::Path<String>,
) -> Result<axum::Json<Value>, StatusCode> {
    let snapshot = orchestration_snapshot(&state)?;
    let run = snapshot
        .runs
        .iter()
        .find(|run| run.run_id == run_id)
        .ok_or(StatusCode::NOT_FOUND)?;
    if run.status != astra_thin_client::SessionRunLifecycleStatus::Cancelled {
        if !run
            .available_actions
            .contains(&astra_thin_client::SessionRunAction::Cancel)
        {
            return Err(StatusCode::CONFLICT);
        }
        state
            .cancelled_runs
            .lock()
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
            .push(run_id.clone());
    }
    Ok(axum::Json(
        serde_json::json!({"run_id": run_id, "status": "cancelled", "execution_settled": true}),
    ))
}

/// Project the fixture's issued request and accepted callback through the
/// actual transcript API. This is wire-fixture evidence, not a persistence test.
async fn handle_transcript(
    State(state): State<ServerState>,
    axum::extract::Path(session_id): axum::extract::Path<String>,
    axum::extract::Query(query): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Result<axum::Json<Value>, StatusCode> {
    if session_id == "mock-session" && query.contains_key("run_id") && !query.contains_key("scope")
    {
        let snapshot = orchestration_snapshot(&state)?;
        let run = snapshot
            .runs
            .iter()
            .find(|run| query.get("run_id") == Some(&run.run_id))
            .ok_or(StatusCode::NOT_FOUND)?;
        let content = if state.scenario == MockScenario::AgentThenComplete {
            "child_evidence_visible: delegated review evidence recorded.".into()
        } else {
            format!(
                "fanout_child_{}_evidence_visible",
                snapshot
                    .runs
                    .iter()
                    .position(|node| node.run_id == run.run_id)
                    .unwrap()
                    + 1
            )
        };
        return Ok(axum::Json(
            serde_json::json!({"session_id": session_id, "items": [{
            "session_id": session_id, "item_seq": 1, "run_id": run.run_id,
            "role": "assistant", "content": content, "tool_calls": [], "tool_result": null,
            "created_at": "2026-10-04T00:00:01Z"
        }], "has_more": false, "next_before_seq": null}),
        ));
    }
    if session_id != "mock-session"
        || query.get("scope").map(String::as_str) != Some("root_conversation")
    {
        return Err(StatusCode::NOT_FOUND);
    }
    let root_run = match state.scenario {
        MockScenario::ToolThenComplete => "mock-run-tool",
        MockScenario::AgentThenComplete => "mock-run-agent-root",
        MockScenario::FanoutThenComplete | MockScenario::FanoutPartialThenComplete => {
            "mock-run-fanout-root"
        }
        _ => return Err(StatusCode::NOT_FOUND),
    };
    let requests = state
        .received_requests
        .lock()
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let issued = state
        .issued_tool_requests
        .lock()
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let callbacks = state
        .tool_results
        .lock()
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let mut items = Vec::new();
    let mut push = |role: &str, content: String, calls: Value, result: Value| {
        items.push(serde_json::json!({
            "session_id": session_id, "item_seq": items.len() + 1, "run_id": root_run,
            "role": role, "content": content, "tool_calls": calls, "tool_result": result,
            "created_at": "2026-10-03T00:00:00Z"
        }));
    };
    if let Some(request) = requests.first() {
        push(
            "user",
            request
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            serde_json::json!([]),
            Value::Null,
        );
    }
    for request in issued.iter().filter(|request| request.run_id == root_run) {
        let Some(callback) = callbacks.iter().find(|callback| {
            callback["request_id"].as_str() == Some(request.request_id.as_str())
                && callback["run_id"].as_str() == Some(request.run_id.as_str())
        }) else {
            continue;
        };
        push(
            "assistant",
            String::new(),
            serde_json::json!([{
                "tool_use_id": request.request_id, "name": request.tool, "arguments": request.args.to_string()
            }]),
            Value::Null,
        );
        push(
            "tool",
            callback["output"].as_str().unwrap_or_default().to_owned(),
            serde_json::json!([]),
            serde_json::json!({
                "tool_use_id": request.request_id, "name": request.tool,
                "status": callback["status"], "duration_ms": callback["duration_ms"]
            }),
        );
    }
    for (run_id, text) in state
        .emitted_text
        .lock()
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .iter()
    {
        if run_id == root_run {
            push(
                "assistant",
                text.clone(),
                serde_json::json!([]),
                Value::Null,
            );
        }
    }
    Ok(axum::Json(
        serde_json::json!({"session_id": session_id, "items": items, "has_more": false, "next_before_seq": null}),
    ))
}

// ─── Public API ───────────────────────────────────────────────────────────────

/// A running mock LLM server. Drop to shut down.
pub struct MockLlmServer {
    pub base_url: String,
    received_requests: Arc<Mutex<Vec<Value>>>,
    tool_results: Arc<Mutex<Vec<Value>>>,
    completed_fanout_children: Arc<AtomicU8>,
    cancelled_runs: Arc<Mutex<Vec<String>>>,
    held_response_release: Option<Arc<tokio::sync::Notify>>,
    _shutdown: tokio::sync::oneshot::Sender<()>,
}

impl MockLlmServer {
    /// Start the mock server on a random free port. Returns immediately.
    pub async fn start(scenario: MockScenario) -> Result<Self, String> {
        Self::start_inner(scenario, false).await
    }

    /// Hold a Server orchestration stream until the test releases its terminal
    /// facts. Partial fanout results are published explicitly by the test.
    pub async fn start_with_held_orchestration(scenario: MockScenario) -> Result<Self, String> {
        if !matches!(
            scenario,
            MockScenario::AgentThenComplete
                | MockScenario::FanoutThenComplete
                | MockScenario::FanoutPartialThenComplete
        ) {
            return Err("held orchestration requires an agent or fanout scenario".to_string());
        }
        Self::start_inner(scenario, true).await
    }

    /// Start the slow-response fixture at an explicit provider boundary.
    /// The test releases the response only after it has observed the UI state
    /// under test, so cancellation coverage cannot race a fixed sleep.
    pub async fn start_with_held_slow_response() -> Result<Self, String> {
        Self::start_inner(MockScenario::Slow, true).await
    }

    async fn start_inner(scenario: MockScenario, hold_response: bool) -> Result<Self, String> {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .map_err(|e| format!("mock server bind failed: {e}"))?;
        let addr: SocketAddr = listener.local_addr().expect("listener has local_addr");
        let base_url = format!("http://127.0.0.1:{}", addr.port());

        let received_requests = Arc::new(Mutex::new(Vec::new()));
        let issued_tool_requests = Arc::new(Mutex::new(Vec::new()));
        let tool_results = Arc::new(Mutex::new(Vec::new()));
        let completed_fanout_children = Arc::new(AtomicU8::new(0));
        let cancelled_runs = Arc::new(Mutex::new(Vec::new()));
        let held_response_release = hold_response.then(|| Arc::new(tokio::sync::Notify::new()));
        let state = ServerState {
            scenario,
            call_count: Arc::new(AtomicU32::new(0)),
            received_requests: received_requests.clone(),
            issued_tool_requests,
            tool_results: tool_results.clone(),
            completed_fanout_children: completed_fanout_children.clone(),
            cancelled_runs: cancelled_runs.clone(),
            callback_ready: Arc::new(tokio::sync::Notify::new()),
            emitted_text: Arc::new(Mutex::new(Vec::new())),
            held_response_release: held_response_release.clone(),
        };

        let app = Router::new()
            .route("/chat/stream", post(handle_chat_turn))
            .route("/tools/result", post(handle_tool_result))
            .route("/sessions", post(handle_create_session))
            .route("/sessions/{session_id}/transcript", get(handle_transcript))
            .route("/sessions/{session_id}/runs", get(handle_run_tree))
            .route("/chat/runs/{run_id}", delete(handle_cancel_run))
            .route("/models", get(handle_models))
            .route("/model-access", get(handle_model_access))
            .route("/model-access/admit", post(handle_model_admission))
            .fallback(handle_unimplemented_mock_route)
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
            received_requests,
            tool_results,
            completed_fanout_children,
            cancelled_runs,
            held_response_release,
            _shutdown: tx,
        })
    }

    /// Return the bounded request history observed by this mock server.
    pub fn received_requests(&self) -> Vec<Value> {
        self.received_requests
            .lock()
            .map(|requests| requests.clone())
            .unwrap_or_default()
    }

    /// Publish the fixture's first two slot results at an explicit test boundary.
    pub fn publish_partial_fanout_results(&self) {
        self.completed_fanout_children
            .store(0b011, Ordering::Release);
    }

    /// Return the bounded callback bodies accepted by the mock server.
    /// Server-owned turns deliver tool outcomes through `/tools/result`, not
    /// through client-supplied conversation history.
    pub fn tool_results(&self) -> Vec<Value> {
        self.tool_results
            .lock()
            .map(|records| records.clone())
            .unwrap_or_default()
    }

    pub fn cancelled_runs(&self) -> Vec<String> {
        self.cancelled_runs
            .lock()
            .expect("fixture cancellation records")
            .clone()
    }

    pub fn release_held_response(&self) {
        self.held_response_release
            .as_ref()
            .expect("mock server was not started with a held response")
            .notify_one();
    }
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::{
        IssuedToolRequestIdentity, MockScenario, body_complete, body_fail, body_malformed_json,
        body_multi_turn, body_rate_limited, body_sse_chunk_split, body_text_only,
        body_tool_then_complete, issued_tool_requests_from_sse, tool_request_for_run,
    };
    use serde_json::Value;

    // Test the SSE body generators directly (no HTTP server needed)

    #[test]
    fn complete_body_contains_required_events() {
        let body = body_complete("test-agent", 1);
        assert!(body.contains("session_info"));
        assert!(body.contains("text_delta"));
        assert!(body.contains("text_done"));
        assert!(body.contains("\"type\":\"done\""));
        assert!(body.contains("\"type\":\"run_finished\""));
        assert!(body.contains("\"type\":\"turn_complete\""));
        assert!(body.contains("\"continuation_owner\":\"server\""));
        assert!(body.contains("\"tool_ledger_receipt\""));
        assert!(body.contains("test-agent"));
    }

    #[test]
    fn tool_rounds_emit_request_then_server_completion() {
        let request = body_tool_then_complete("coder", 1);
        let completion = body_tool_then_complete("coder", 2);
        assert!(request.contains("tool_call_start"));
        assert!(request.contains("write_file"));
        assert!(!request.contains("tool_result"));
        assert!(!request.contains("text_done"));
        assert!(completion.contains("text_done"));
        assert!(!completion.contains("tool_call_start"));
        assert!(completion.contains("\"type\":\"done\""));
    }

    #[tokio::test]
    async fn server_agent_queries_preserve_identity_and_cancel_only_selected_run() {
        use astra_thin_client::{
            SessionRunLifecycleStatus, SessionTranscriptReadScope, ThinClient,
        };
        let server = super::MockLlmServer::start_with_held_orchestration(
            MockScenario::FanoutPartialThenComplete,
        )
        .await
        .unwrap();
        let client = ThinClient::new(&server.base_url, None).unwrap();
        assert!(
            client
                .get_session_run_tree(Some("fixture"), "another-session", 200)
                .await
                .is_err()
        );
        assert!(
            client
                .get_session_run_tree(Some("fixture"), "mock-session", 200)
                .await
                .unwrap()
                .runs
                .is_empty()
        );
        assert!(
            client
                .cancel_run(Some("fixture"), "mock-run-fanout-child-2")
                .await
                .is_err()
        );
        let stream = reqwest::Client::new()
            .post(format!("{}/chat/stream", server.base_url))
            .json(&serde_json::json!({"agent_id": "astra-cli", "message": "launch the review"}))
            .send()
            .await
            .unwrap();
        let before = client
            .get_session_run_tree(Some("fixture"), "mock-session", 200)
            .await
            .unwrap();
        assert_eq!(before.runs.len(), 3);
        let selected = &before.runs[2].run_id;
        let page = client
            .get_session_transcript(
                Some("fixture"),
                "mock-session",
                SessionTranscriptReadScope::Run(selected),
                None,
                200,
            )
            .await
            .unwrap();
        assert_eq!(page.items.len(), 1);
        assert_eq!(page.items[0].run_id.as_deref(), Some(selected.as_str()));
        assert!(
            page.items[0]
                .content
                .contains("fanout_child_3_evidence_visible")
        );
        for (session, run) in [
            ("wrong-session", selected.as_str()),
            ("mock-session", "wrong-run"),
        ] {
            assert!(
                client
                    .get_session_transcript(
                        Some("fixture"),
                        session,
                        SessionTranscriptReadScope::Run(run),
                        None,
                        200
                    )
                    .await
                    .is_err()
            );
        }
        assert!(
            client
                .cancel_run(Some("fixture"), "wrong-run")
                .await
                .is_err()
        );
        let receipt = client.cancel_run(Some("fixture"), selected).await.unwrap();
        assert_eq!(receipt["execution_settled"], true);
        client.cancel_run(Some("fixture"), selected).await.unwrap();
        let after = client
            .get_session_run_tree(Some("fixture"), "mock-session", 200)
            .await
            .unwrap();
        assert_ne!(before.snapshot_revision, after.snapshot_revision);
        assert_eq!(&before.runs[..2], &after.runs[..2]);
        assert_eq!(after.runs[2].status, SessionRunLifecycleStatus::Cancelled);
        assert!(after.runs[2].available_actions.is_empty());
        assert!(after.runs[2].run_event_high_watermark > before.runs[2].run_event_high_watermark);
        assert_eq!(server.cancelled_runs(), std::slice::from_ref(selected));
        let retained = client
            .get_session_transcript(
                Some("fixture"),
                "mock-session",
                SessionTranscriptReadScope::Run(selected),
                None,
                200,
            )
            .await
            .unwrap();
        assert_eq!(retained.items[0].content, page.items[0].content);
        let failed_run = &before.runs[1].run_id;
        let read_failed = || {
            client.get_session_transcript(
                Some("fixture"),
                "mock-session",
                SessionTranscriptReadScope::Run(failed_run),
                None,
                200,
            )
        };
        let before_failure = read_failed().await.unwrap();
        server
            .completed_fanout_children
            .store(1 << 1, std::sync::atomic::Ordering::Release);
        let after_failure = read_failed().await.unwrap();
        assert_eq!(
            serde_json::to_value(before_failure).unwrap(),
            serde_json::to_value(after_failure).unwrap()
        );
        let failed = client
            .get_session_run_tree(Some("fixture"), "mock-session", 200)
            .await
            .unwrap();
        assert_eq!(failed.runs[1].status, SessionRunLifecycleStatus::Failed);
        assert_eq!(
            failed.runs[1].error_message.as_deref(),
            Some("fanout_child_2_failed_with_distinct_cause")
        );
        assert!(
            client
                .cancel_run(Some("fixture"), failed_run)
                .await
                .is_err()
        );
        drop(stream);
    }

    #[tokio::test]
    async fn server_orchestration_uses_one_stream_without_edge_callbacks() {
        for scenario in [
            MockScenario::AgentThenComplete,
            MockScenario::FanoutThenComplete,
            MockScenario::FanoutPartialThenComplete,
        ] {
            let server = super::MockLlmServer::start_inner(scenario, true)
                .await
                .unwrap();
            let mut response = reqwest::Client::new()
                .post(format!("{}/chat/stream", server.base_url))
                .json(&serde_json::json!({"agent_id":"astra-cli", "message":"review"}))
                .send()
                .await
                .unwrap();
            let mut wire = String::new();
            while !wire.contains("tool_call_end") {
                wire.push_str(
                    std::str::from_utf8(&response.chunk().await.unwrap().expect("launch receipt"))
                        .unwrap(),
                );
            }
            assert!(!wire.contains("tool_request"));
            assert!(!wire.contains("turn_complete"));
            assert!(
                tokio::time::timeout(std::time::Duration::from_millis(30), response.chunk())
                    .await
                    .is_err()
            );
            let api = astra_thin_client::ThinClient::new(&server.base_url, None).unwrap();
            let root = api
                .get_session_transcript(
                    Some("fixture"),
                    "mock-session",
                    astra_thin_client::SessionTranscriptReadScope::RootConversation,
                    None,
                    200,
                )
                .await
                .unwrap();
            let root_id = if scenario == MockScenario::AgentThenComplete {
                "mock-run-agent-root"
            } else {
                "mock-run-fanout-root"
            };
            assert_eq!(
                root.items.len(),
                2,
                "the active root has committed user input and launch acknowledgement"
            );
            assert!(
                root.items
                    .iter()
                    .all(|item| item.run_id.as_deref() == Some(root_id))
            );
            assert!(root.items[1].content.contains(
                if scenario == MockScenario::AgentThenComplete {
                    "Parent acknowledged"
                } else {
                    "Three mock reviews"
                }
            ));
            let before = api
                .get_session_run_tree(Some("fixture"), "mock-session", 200)
                .await
                .unwrap();
            let selected = before.runs.last().unwrap().run_id.clone();
            api.cancel_run(Some("fixture"), &selected).await.unwrap();
            server.release_held_response();
            while let Some(chunk) = response.chunk().await.unwrap() {
                wire.push_str(std::str::from_utf8(&chunk).unwrap());
            }
            let events: Vec<Value> = parse_sse_events(&wire)
                .iter()
                .map(|event| serde_json::from_str(event).unwrap())
                .collect();
            let terminal: Vec<_> = events
                .iter()
                .filter(|event| event["type"] == "turn_complete")
                .collect();
            assert_eq!(terminal.len(), 1);
            assert_eq!(terminal[0]["tool_calls_count"], 1);
            assert!(
                events
                    .iter()
                    .any(|event| event["type"] == "agent_live_event"
                        && event["run_id"] == selected
                        && event["termination"] == "cancelled")
            );
            assert!(!events.iter().any(|event| event["run_id"] == selected && event["termination"] == "completed"));
            assert_eq!(server.received_requests().len(), 1);
            assert!(server.tool_results().is_empty());
            assert!(super::issued_tool_requests_from_sse(&wire).is_empty());
        }
    }

    #[tokio::test]
    async fn orchestration_stream_waits_for_exact_callbacks_and_closes_real_counts() {
        for scenario in [MockScenario::ToolThenComplete] {
            let server = super::MockLlmServer::start(scenario).await.unwrap();
            let client = reqwest::Client::new();
            let mut response = client
                .post(format!("{}/chat/stream", server.base_url))
                .json(&serde_json::json!({"agent_id": "astra-cli", "message": "launch the review"}))
                .send()
                .await
                .unwrap();
            let expected = 1;
            let mut full = String::new();
            for _ in 0..expected {
                let mut stage = String::new();
                while super::issued_tool_requests_from_sse(&stage).is_empty() {
                    let chunk = response
                        .chunk()
                        .await
                        .unwrap()
                        .expect("tool request before EOF");
                    stage.push_str(std::str::from_utf8(&chunk).unwrap());
                }
                let identity = super::issued_tool_requests_from_sse(&stage).remove(0);
                assert!(!stage.contains("turn_complete"));
                assert!(
                    tokio::time::timeout(std::time::Duration::from_millis(30), response.chunk())
                        .await
                        .is_err()
                );
                let make_result = |request_id: String| {
                    astra_thin_client::ToolResultRequest::new_with_hash(
                        astra_thin_client::ToolResultRequestParts {
                            session_id: identity.session_id.clone(),
                            run_id: identity.run_id.clone(),
                            turn_chain_id: identity.turn_chain_id.clone(),
                            request_id,
                            edge_agent_id: "edge-fixture".to_owned(),
                            status: "completed".to_owned(),
                            output: "accepted launch receipt".to_owned(),
                            duration_ms: 1,
                            tool_result_fields: None,
                        },
                    )
                };
                let rejected = client
                    .post(format!("{}/tools/result", server.base_url))
                    .header(astra_thin_client::ASTRA_EDGE_ID_HEADER, "edge-fixture")
                    .json(&make_result("unissued".to_owned()))
                    .send()
                    .await
                    .unwrap();
                assert_eq!(rejected.status(), reqwest::StatusCode::BAD_REQUEST);
                assert!(
                    tokio::time::timeout(std::time::Duration::from_millis(30), response.chunk())
                        .await
                        .is_err()
                );
                for _ in 0..2 {
                    let accepted = client
                        .post(format!("{}/tools/result", server.base_url))
                        .header(astra_thin_client::ASTRA_EDGE_ID_HEADER, "edge-fixture")
                        .json(&make_result(identity.request_id.clone()))
                        .send()
                        .await
                        .unwrap();
                    assert!(accepted.status().is_success());
                }
                full.push_str(&stage);
            }
            while let Some(chunk) = response.chunk().await.unwrap() {
                full.push_str(std::str::from_utf8(&chunk).unwrap());
            }
            let events: Vec<Value> = parse_sse_events(&full)
                .iter()
                .map(|event| serde_json::from_str(event).unwrap())
                .collect();
            let mut accum = astra_turn_core::chat_turn_sse_dispatch::ChatTurnSseAccum::default();
            for event in &events {
                astra_turn_core::chat_turn_sse_dispatch::dispatch_chat_turn_sse_event_block(
                    &super::sse_line(event),
                    &mut accum,
                    &mut Vec::new(),
                );
            }
            assert!(accum.usage_is_run_total);
            assert_eq!(accum.qualified_usage.unwrap().input_tokens(), Some(350));
            assert_eq!(accum.current_request_input_tokens, Some(200));
            assert_eq!(accum.current_request_usage.unwrap().output_tokens, 50);
            let terminals: Vec<_> = events
                .iter()
                .filter(|event| event["type"] == "turn_complete")
                .collect();
            assert_eq!(terminals.len(), 1);
            let terminal = terminals[0];
            assert_eq!(terminal["tool_calls_count"], expected);
            assert_eq!(terminal["llm_rounds"], expected + 1);
            let receipt: astra_turn_core::tool_ledger_receipt::ToolLedgerReceipt =
                serde_json::from_value(terminal["tool_ledger_receipt"].clone()).unwrap();
            assert!(receipt.is_complete());
            assert_eq!(receipt.attempted, expected);
            assert_eq!(server.received_requests().len(), 1);
            if scenario == MockScenario::ToolThenComplete {
                let page: astra_thin_client::SessionTranscriptPage = client
                    .get(format!(
                        "{}/sessions/mock-session/transcript?scope=root_conversation&limit=200",
                        server.base_url
                    ))
                    .send()
                    .await
                    .unwrap()
                    .json()
                    .await
                    .unwrap();
                assert_eq!(page.items.len(), 4);
                let call = &page.items[1].tool_calls[0];
                let result = page.items[2].tool_result.as_ref().unwrap();
                assert_eq!(call.tool_use_id, result.tool_use_id);
                assert_eq!(call.name, "write_file");
                assert_eq!(result.status.as_deref(), Some("completed"));
                assert_eq!(page.items[2].content, "accepted launch receipt");
                assert!(page.items[3].content.contains("wrote the requested file"));
            }
        }
    }

    #[tokio::test]
    async fn failed_callback_keeps_usage_and_tool_evidence_without_success_text() {
        let server = super::MockLlmServer::start(MockScenario::ToolThenComplete)
            .await
            .unwrap();
        let client = astra_core::net::client_builder_for_target(&server.base_url)
            .build()
            .unwrap();
        let mut response = client
            .post(format!("{}/chat/stream", server.base_url))
            .json(&serde_json::json!({"agent_id": "astra-cli", "message": "write the file"}))
            .send()
            .await
            .unwrap();
        let mut observed = String::new();
        while super::issued_tool_requests_from_sse(&observed).is_empty() {
            observed
                .push_str(std::str::from_utf8(&response.chunk().await.unwrap().unwrap()).unwrap());
        }
        let request = super::issued_tool_requests_from_sse(&observed).remove(0);
        let result = astra_thin_client::ToolResultRequest::new_with_hash(
            astra_thin_client::ToolResultRequestParts {
                session_id: request.session_id,
                run_id: request.run_id,
                turn_chain_id: request.turn_chain_id,
                request_id: request.request_id,
                edge_agent_id: "edge-fixture".into(),
                status: "failed".into(),
                output: "publication rejected".into(),
                duration_ms: 1,
                tool_result_fields: None,
            },
        );
        assert!(
            client
                .post(format!("{}/tools/result", server.base_url))
                .header(astra_thin_client::ASTRA_EDGE_ID_HEADER, "edge-fixture")
                .json(&result)
                .send()
                .await
                .unwrap()
                .status()
                .is_success()
        );
        while let Some(chunk) = response.chunk().await.unwrap() {
            observed.push_str(std::str::from_utf8(&chunk).unwrap());
        }
        assert!(observed.contains("tool callback failed or timed out"));
        assert!(!observed.contains("turn_complete"));
        let mut accum = astra_turn_core::chat_turn_sse_dispatch::ChatTurnSseAccum::default();
        for event in parse_sse_events(&observed) {
            astra_turn_core::chat_turn_sse_dispatch::dispatch_chat_turn_sse_event_block(
                &format!("data: {event}\n\n"),
                &mut accum,
                &mut Vec::new(),
            );
        }
        assert_eq!(accum.qualified_usage.unwrap().input_tokens(), Some(150));
        let page: astra_thin_client::SessionTranscriptPage = client
            .get(format!(
                "{}/sessions/mock-session/transcript?scope=root_conversation&limit=200",
                server.base_url
            ))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(page.items.len(), 3);
        assert_eq!(
            page.items[2]
                .tool_result
                .as_ref()
                .unwrap()
                .status
                .as_deref(),
            Some("failed")
        );
        assert_eq!(page.items[2].content, "publication rejected");
        assert!(
            !page
                .items
                .iter()
                .any(|item| item.content.contains("wrote the requested file"))
        );
    }

    #[test]
    fn callback_identity_is_derived_from_the_exact_issued_tool_request() {
        let body = tool_request_for_run(
            "request-dynamic",
            "write_file",
            serde_json::json!({"path": "out.txt", "content": "ok"}),
            "run-dynamic",
            "turn-chain-dynamic",
        );
        let issued = issued_tool_requests_from_sse(&body);
        assert_eq!(
            issued,
            vec![IssuedToolRequestIdentity {
                session_id: "mock-session".to_string(),
                run_id: "run-dynamic".to_string(),
                turn_chain_id: "turn-chain-dynamic".to_string(),
                request_id: "request-dynamic".to_string(),
                tool: "write_file".to_owned(),
                args: serde_json::json!({"path": "out.txt", "content": "ok"}),
            }]
        );

        let result = astra_thin_client::ToolResultRequest::new_with_hash(
            astra_thin_client::ToolResultRequestParts {
                session_id: "mock-session".to_string(),
                run_id: "run-dynamic".to_string(),
                turn_chain_id: "turn-chain-dynamic".to_string(),
                request_id: "request-dynamic".to_string(),
                edge_agent_id: "edge-1".to_string(),
                status: "completed".to_string(),
                output: "done".to_string(),
                duration_ms: 1,
                tool_result_fields: None,
            },
        );
        assert!(issued[0].matches_result(&result));

        let foreign_result = astra_thin_client::ToolResultRequest::new_with_hash(
            astra_thin_client::ToolResultRequestParts {
                session_id: "mock-session".to_string(),
                run_id: "run-dynamic".to_string(),
                turn_chain_id: "turn-chain-dynamic".to_string(),
                request_id: "request-never-issued".to_string(),
                edge_agent_id: "edge-1".to_string(),
                status: "completed".to_string(),
                output: "done".to_string(),
                duration_ms: 1,
                tool_result_fields: None,
            },
        );
        assert!(!issued[0].matches_result(&foreign_result));
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
                MockScenario::parse(name).is_some(),
                "scenario '{name}' not parseable"
            );
        }
        assert!(MockScenario::parse("nonexistent").is_none());
        // Aliases
        assert_eq!(
            MockScenario::parse("tool"),
            Some(MockScenario::ToolThenComplete)
        );
        assert_eq!(MockScenario::parse("multi"), Some(MockScenario::MultiTurn));
        assert_eq!(MockScenario::parse("error"), Some(MockScenario::Fail));
        assert_eq!(
            MockScenario::parse("chunk_split"),
            Some(MockScenario::SseChunkSplit)
        );
        assert_eq!(
            MockScenario::parse("malformed"),
            Some(MockScenario::MalformedJson)
        );
        assert_eq!(MockScenario::parse("429"), Some(MockScenario::RateLimited));
        assert_eq!(
            MockScenario::parse("no_tools"),
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
        assert!(
            !body.contains("\"type\":\"turn_complete\""),
            "text_only must remain a missing-terminal negative control"
        );
        assert!(
            !body.contains("\"type\":\"run_finished\""),
            "text_only must not fabricate durable Server completion"
        );
        // It still has a complete provider response shape for inference-only
        // callers, but it is not a successful Server-owned turn.
        let events = parse_sse_events(&body);
        let kinds: Vec<String> = events
            .iter()
            .filter_map(|e| serde_json::from_str::<Value>(e).ok())
            .filter_map(|v| v["type"].as_str().map(str::to_string))
            .collect();
        assert!(kinds.iter().any(|kind| kind == "session_info"));
        assert!(kinds.iter().any(|kind| kind == "text_done"));
        assert!(kinds.iter().any(|kind| kind == "done"));
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
