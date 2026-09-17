//! Mock LLM server for end-to-end testing of team orchestration without real LLM calls.
//!
//! Starts an in-process axum HTTP server that responds to `POST /chat/stream` with
//! pre-scripted SSE streams. Every other part of the system (worktrees, progress
//! channels, delegation engine, tool execution, journal) runs for real.
//!
//! # Usage
//! ```text
//! /team run dev "build login page" --mock complete
//! /team run dev "build login page" --mock tool_then_complete
//! /team run dev "build login page" --mock multi_turn
//! /team run dev "build login page" --mock fail
//! /team run dev "build login page" --mock slow
//! ```

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU8, AtomicU32, Ordering};

use axum::Router;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
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
    /// Agent remains pending long enough for cancellation journeys to prove
    /// that Ctrl+C, rather than a naturally completed response, settles the turn.
    CancellationPending,
    /// Root activates and calls one foreground child agent, then synthesizes
    /// the child's completion.
    AgentThenComplete,
    /// Root launches one foreground three-slot fanout. Individual slots
    /// settle at different times; the root remains blocked until the full
    /// group result is available and then synthesizes exactly once.
    FanoutThenComplete,
    /// Same structured fan-in, but the second child fails. The parent still
    /// receives one canonical 2/3 aggregate after the whole group settles.
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
            Self::Complete => "immediate completion (no tools)",
            Self::ToolThenComplete => "one write_file tool call, then completion",
            Self::MultiTurn => "two LLM turns: think then complete",
            Self::Fail => "agent returns error",
            Self::Slow => "3s delay before response (tests progress display)",
            Self::CancellationPending => "pending response for cancellation journeys",
            Self::AgentThenComplete => "one foreground child agent, then parent synthesis",
            Self::FanoutThenComplete => {
                "three foreground fanout slots, one structured parent synthesis"
            }
            Self::FanoutPartialThenComplete => {
                "three foreground fanout slots, one child failure, one parent synthesis"
            }
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
                "cancellation_pending",
                "pending response for cancellation journeys",
            ),
            (
                "agent_then_complete",
                "one foreground child agent, then parent synthesis",
            ),
            (
                "fanout_then_complete",
                "three foreground fanout slots, one structured parent synthesis",
            ),
            (
                "fanout_partial_then_complete",
                "three foreground fanout slots, one child failure, one parent synthesis",
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
        "execution_timeout_ms": 300_000,
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
    // A client-side tool is a two-turn protocol: the first model response
    // asks the host to execute it; the next response sees that result and
    // completes. Returning a synthetic tool result alongside the request (or
    // issuing the same request on every round) makes a real host loop forever.
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
    s.push_str(&done_event(200));
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

pub const AGENT_JOURNEY_CHILD_TASK: &str =
    "Inspect the assigned work and return one evidence-backed finding.";

fn is_agent_journey_child_request(body: &Value) -> bool {
    body.get("message").and_then(Value::as_str) == Some(AGENT_JOURNEY_CHILD_TASK)
        && body.pointer("/context/agent_type").and_then(Value::as_str) == Some("general-purpose")
}

fn root_has_tool_result(body: &Value, tool_name: &str) -> bool {
    // Tool results can be executed by a pre-resolved runtime binding, in
    // which case their transport-level `name` is `pre_resolved`. The stable
    // join is the tool_call_id emitted by the preceding assistant message,
    // not a duplicated display name on the result envelope.
    let matching_call_ids = body
        .get("messages")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .flat_map(|message| {
            message
                .get("tool_calls")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
        })
        .filter(|call| call.pointer("/function/name").and_then(Value::as_str) == Some(tool_name))
        .filter_map(|call| call.get("id").and_then(Value::as_str))
        .collect::<std::collections::HashSet<_>>();

    let callback_has_result = body
        .get("tool_results")
        .and_then(Value::as_array)
        .is_some_and(|results| {
            results.iter().any(|result| {
                result.get("name").and_then(Value::as_str) == Some(tool_name)
                    || result.get("tool").and_then(Value::as_str) == Some(tool_name)
                    || result
                        .get("tool_call_id")
                        .and_then(Value::as_str)
                        .is_some_and(|call_id| matching_call_ids.contains(call_id))
            })
        });
    if callback_has_result {
        return true;
    }

    body.get("messages")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .any(|message| {
            message.get("role").and_then(Value::as_str) == Some("tool")
                && (message.get("_tool_name").and_then(Value::as_str) == Some(tool_name)
                    || message
                        .get("tool_call_id")
                        .and_then(Value::as_str)
                        .is_some_and(|call_id| matching_call_ids.contains(call_id)))
        })
}

async fn body_agent_then_complete(body: &Value) -> String {
    body_agent_then_complete_with_callbacks(body, None).await
}

async fn body_agent_then_complete_with_callbacks(
    body: &Value,
    callbacks: Option<&Arc<Mutex<Vec<Value>>>>,
) -> String {
    if is_agent_journey_child_request(body) {
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
        let message = "child_evidence_visible: delegated review completed successfully.";
        let mut stream = session_info("mock-run-agent-child");
        stream.push_str(&text_delta(message));
        stream.push_str(&text_done(message));
        stream.push_str(&done_event(100));
        return stream;
    }

    let tool_search_done = callbacks
        .map(|callbacks| callback_recorded(callbacks, "call-activate-agent"))
        .unwrap_or_else(|| root_has_tool_result(body, "tool_search"));
    let agent_done = callbacks
        .map(|callbacks| callback_recorded(callbacks, "call-spawn-child"))
        .unwrap_or_else(|| root_has_tool_result(body, "agent"));

    match (tool_search_done, agent_done) {
        (false, false) => {
            let mut stream = session_info("mock-run-agent-root");
            let args = serde_json::json!({"query": "select:agent"});
            stream.push_str(&tool_call_start(
                "call-activate-agent",
                "tool_search",
                args.clone(),
            ));
            stream.push_str(&tool_request_for_run(
                "call-activate-agent",
                "tool_search",
                args,
                "mock-run-agent-root",
                "mock-turn-chain-agent-root",
            ));
            stream.push_str(&done_event(40));
            stream
        }
        (true, false) => {
            let mut stream = session_info("mock-run-agent-root");
            let args = serde_json::json!({
                "action": "spawn",
                "description": "Mock child review",
                "prompt": AGENT_JOURNEY_CHILD_TASK,
                "agent_type": "general-purpose"
            });
            stream.push_str(&tool_call_start("call-spawn-child", "agent", args.clone()));
            stream.push_str(&tool_request_for_run(
                "call-spawn-child",
                "agent",
                args,
                "mock-run-agent-root",
                "mock-turn-chain-agent-root",
            ));
            stream.push_str(&done_event(80));
            stream
        }
        (_, true) => {
            let message = "Parent synthesized the child evidence and completed the task.";
            let mut stream = session_info("mock-run-agent-root");
            stream.push_str(&text_delta(message));
            stream.push_str(&text_done(message));
            stream.push_str(&done_event(140));
            stream
        }
    }
}

pub const FANOUT_JOURNEY_CHILD_TASKS: [&str; 3] = [
    "Inspect storage behavior and return one evidence-backed finding.",
    "Inspect runtime behavior and return one evidence-backed finding.",
    "Inspect user experience and return one evidence-backed finding.",
];
pub const FANOUT_JOURNEY_STATUS_QUESTION: &str = "what_background_work_is_running";

fn edge_profile(body: &Value) -> Option<&Value> {
    body.get("edge_profile")
        .or_else(|| body.pointer("/context/edge_profile"))
}

fn server_edge_profile(body: &Value) -> Option<&Value> {
    body.pointer("/context/edge_profile")
}

fn server_message(body: &Value) -> Option<&str> {
    body.get("message").and_then(Value::as_str)
}

fn server_fanout_journey_child_index(body: &Value) -> Option<usize> {
    body.get("context")?;
    let prompt = server_message(body)?;
    FANOUT_JOURNEY_CHILD_TASKS
        .iter()
        .position(|candidate| *candidate == prompt)
}

fn latest_user_message(body: &Value) -> Option<&str> {
    body.get("message").and_then(Value::as_str).or_else(|| {
        body.get("messages")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .rev()
            .find(|message| message.get("role").and_then(Value::as_str) == Some("user"))
            .and_then(|message| message.get("content"))
            .and_then(Value::as_str)
    })
}

fn fanout_journey_child_index(body: &Value) -> Option<usize> {
    let prompt = latest_user_message(body)?;
    FANOUT_JOURNEY_CHILD_TASKS
        .iter()
        .position(|candidate| *candidate == prompt)
}

async fn body_fanout_then_complete(
    body: &Value,
    failed_child: Option<usize>,
    completed_children: &AtomicU8,
    held_response_release: Option<&tokio::sync::Notify>,
) -> String {
    body_fanout_then_complete_with_callbacks(
        body,
        failed_child,
        completed_children,
        held_response_release,
        None,
        None,
        None,
    )
    .await
}

async fn body_fanout_then_complete_with_callbacks(
    body: &Value,
    failed_child: Option<usize>,
    completed_children: &AtomicU8,
    held_response_release: Option<&tokio::sync::Notify>,
    callbacks: Option<&Arc<Mutex<Vec<Value>>>>,
    guidance_pending: Option<&Arc<AtomicU8>>,
    guidance_requests: Option<&Arc<Mutex<Vec<Value>>>>,
) -> String {
    let child_index = if callbacks.is_some() {
        server_fanout_journey_child_index(body)
    } else {
        fanout_journey_child_index(body)
    };
    if let Some(index) = child_index {
        match index {
            0 => tokio::time::sleep(std::time::Duration::from_millis(250)).await,
            1 => tokio::time::sleep(std::time::Duration::from_millis(700)).await,
            _ => match held_response_release {
                Some(release) => release.notified().await,
                None => tokio::time::sleep(std::time::Duration::from_secs(6)).await,
            },
        }
        if failed_child == Some(index) {
            let mut stream = session_info(&format!("mock-run-fanout-child-{index}"));
            stream.push_str(&error_event(&format!(
                "fanout_child_{}_failed_with_distinct_cause",
                index + 1
            )));
            completed_children.fetch_or(1 << index, Ordering::Release);
            return stream;
        }
        let message = format!("fanout_child_{}_evidence_visible", index + 1);
        let mut stream = session_info(&format!("mock-run-fanout-child-{index}"));
        stream.push_str(&text_delta(&message));
        stream.push_str(&text_done(&message));
        stream.push_str(&done_event(100));
        completed_children.fetch_or(1 << index, Ordering::Release);
        return stream;
    }

    // Runtime reconciliation is a typed, already-authorized continuation of
    // the existing fanout. Handle it before ordinary tool discovery so this
    // journey fails if a terminal notification accidentally re-enters the
    // delegation bootstrap and creates the work again.
    let latest_user = if callbacks.is_some() {
        server_message(body).unwrap_or_default()
    } else {
        latest_user_message(body).unwrap_or_default()
    };
    if latest_user == astra_turn_core::chat_turn_edge_profile::RUNTIME_RECONCILIATION_USER_ENVELOPE
    {
        let message = "Parent reconciled one terminal fanout group exactly once.";
        let mut stream = session_info("mock-run-fanout-root");
        stream.push_str(&text_delta(message));
        stream.push_str(&text_done(message));
        stream.push_str(&done_event(140));
        return stream;
    }
    if latest_user == FANOUT_JOURNEY_STATUS_QUESTION {
        let request = body.to_string();
        let active_work = if callbacks.is_some() {
            server_edge_profile(body)
        } else {
            edge_profile(body)
        }
        .and_then(|profile| profile.get("runtime_volatile_injections"))
        .and_then(serde_json::Value::as_array)
        .and_then(|injections| {
            injections
                .iter()
                .find(|injection| injection["kind"] == "active_work_snapshot")
        });
        let payload = active_work.map(|injection| &injection["payload"]);
        let guidance_snapshot = payload
            .and_then(|payload| payload["snapshots"].as_array())
            .and_then(|snapshots| {
                snapshots.iter().find(|snapshot| {
                    snapshot["schema"] == "active_work_snapshot.v1"
                        && snapshot["authority"] == "run_control_provider"
                })
            });
        let observation = payload
            .and_then(|payload| payload["work_unit_observations"].as_array())
            .or_else(|| {
                guidance_snapshot.and_then(|snapshot| snapshot["work_unit_observations"].as_array())
            })
            .and_then(|observations| {
                observations
                    .iter()
                    .find(|observation| observation["id"] == "mock-review-group")
            });
        let direct_contract = active_work.is_some_and(|injection| {
            injection["delivery_class"] == "required_context"
                && injection["payload"]["authority"] == "runtime_producer"
                && injection["payload"]["schema"] == "active_work_snapshot.v1"
        });
        let guidance_contract = active_work.is_some_and(|injection| {
            injection["delivery_class"] == "required_context"
                && injection["payload"]["authority"] == "runtime_required_context"
                && injection["payload"]["schema"] == "active_work_guidance_context.v1"
                && guidance_snapshot.is_some()
        });
        let has_group_truth = (direct_contract || guidance_contract)
            && observation.is_some_and(|observation| observation["kind"] == "agent_fanout");
        let message = if has_group_truth
            && request.contains("superseded_by_newer_producer_observation")
            && observation.is_some_and(|observation| observation["status"] == "completed")
        {
            "Astra knows Three mock reviews completed as one foreground work group. Parent synthesized one terminal fanout group exactly once."
        } else if has_group_truth {
            "Astra knows Three mock reviews are running as one background work group."
        } else {
            "ERROR: authoritative background work snapshot is missing."
        };
        let mut stream = session_info("mock-run-fanout-root");
        stream.push_str(&text_delta(message));
        stream.push_str(&text_done(message));
        stream.push_str(&done_event(140));
        return stream;
    }

    let tool_search_done = callbacks
        .map(|callbacks| callback_recorded(callbacks, "call-activate-fanout"))
        .unwrap_or_else(|| root_has_tool_result(body, "tool_search"));
    if !tool_search_done {
        let mut stream = session_info("mock-run-fanout-root");
        let args = serde_json::json!({"query": "select:agent_fanout"});
        stream.push_str(&tool_call_start(
            "call-activate-fanout",
            "tool_search",
            args.clone(),
        ));
        stream.push_str(&tool_request_for_run(
            "call-activate-fanout",
            "tool_search",
            args,
            "mock-run-fanout-root",
            "mock-turn-chain-fanout-root",
        ));
        stream.push_str(&done_event(40));
        return stream;
    }
    let fanout_done = callbacks
        .map(|callbacks| callback_recorded(callbacks, "call-start-fanout"))
        .unwrap_or_else(|| root_has_tool_result(body, "agent_fanout"));
    if !fanout_done {
        let mut stream = session_info("mock-run-fanout-root");
        let args = serde_json::json!({
            "action": "start",
            "group_id": "mock-review-group",
            "title": "Three mock reviews",
            "target_count": 3,
            // This is the production-shaped review fanout: qualitative
            // complexity supplies a renewable persona slice, not a numeric
            // child hard limit. The PTY journey below therefore guards the
            // same code-review/deep configuration used by real audits.
            "defaults": {"agent_type": "code-review", "complexity": "deep"},
            "slots": FANOUT_JOURNEY_CHILD_TASKS.iter().enumerate().map(|(index, prompt)| {
                serde_json::json!({
                    "id": format!("review-{}", index + 1),
                    "description": format!("Mock review {}", index + 1),
                    "prompt": prompt,
                })
            }).collect::<Vec<_>>(),
        });
        stream.push_str(&tool_call_start(
            "call-start-fanout",
            "agent_fanout",
            args.clone(),
        ));
        stream.push_str(&tool_request_for_run(
            "call-start-fanout",
            "agent_fanout",
            args,
            "mock-run-fanout-root",
            "mock-turn-chain-fanout-root",
        ));
        stream.push_str(&done_event(80));
        return stream;
    }

    let guidance_was_accepted = guidance_pending.is_some_and(|pending| {
        pending
            .compare_exchange(1, 2, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    });
    let message = if guidance_was_accepted {
        "Astra knows Three mock reviews completed as one foreground work group. Parent synthesized one terminal fanout group exactly once."
    } else if failed_child.is_some() {
        "Parent synthesized the available 2/3 fanout evidence exactly once."
    } else {
        "Parent synthesized one terminal fanout group exactly once."
    };
    let mut stream = session_info("mock-run-fanout-root");
    if guidance_was_accepted
        && let Some(request) =
            guidance_requests.and_then(|requests| requests.lock().ok()?.last().cloned())
    {
        stream.push_str(&user_intent_applied_event(
            "mock-run-fanout-root",
            &request,
            1,
        ));
    }
    stream.push_str(&text_delta(message));
    stream.push_str(&text_done(message));
    stream.push_str(&done_event(140));
    stream
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

fn callback_recorded(callbacks: &Arc<Mutex<Vec<Value>>>, request_id: &str) -> bool {
    callbacks
        .lock()
        .map(|records| {
            records.iter().any(|record| {
                record.get("request_id").and_then(Value::as_str) == Some(request_id)
                    && record.get("status").and_then(Value::as_str) == Some("completed")
            })
        })
        .unwrap_or(false)
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct IssuedToolRequestIdentity {
    session_id: String,
    run_id: String,
    turn_chain_id: String,
    request_id: String,
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

fn user_intent_applied_event(run_id: &str, request: &Value, event_index: usize) -> String {
    let content = request
        .pointer("/input/content")
        .and_then(Value::as_str)
        .unwrap_or_default();
    sse_line(&serde_json::json!({
        "type": "user_intent_applied",
        "run_id": run_id,
        "intent_id": request.get("intent_id").and_then(Value::as_str).unwrap_or_default(),
        "delivery": "guide_current_run",
        "status": "applied",
        "event_index": event_index,
        "index": event_index,
        "content": content,
    }))
}

#[derive(Clone)]
struct ServerState {
    scenario: MockScenario,
    call_count: Arc<AtomicU32>,
    received_requests: Arc<Mutex<Vec<Value>>>,
    issued_tool_requests: Arc<Mutex<Vec<IssuedToolRequestIdentity>>>,
    tool_results: Arc<Mutex<Vec<Value>>>,
    guidance_requests: Arc<Mutex<Vec<Value>>>,
    guidance_pending: Arc<AtomicU8>,
    completed_fanout_children: Arc<AtomicU8>,
    held_response_release: Option<Arc<tokio::sync::Notify>>,
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

    let sse_body = match state.scenario {
        MockScenario::Complete => body_complete(&agent_id, turn),
        MockScenario::ToolThenComplete => body_tool_then_complete(&agent_id, turn),
        MockScenario::MultiTurn => body_multi_turn(&agent_id, turn),
        MockScenario::Fail => body_fail(&agent_id, turn),
        MockScenario::Slow => {
            body_slow(&agent_id, turn, state.held_response_release.as_deref()).await
        }
        MockScenario::CancellationPending => body_cancellation_pending(&agent_id, turn).await,
        MockScenario::AgentThenComplete => {
            body_agent_then_complete_with_callbacks(&request_body, Some(&state.tool_results)).await
        }
        MockScenario::FanoutThenComplete => {
            body_fanout_then_complete_with_callbacks(
                &request_body,
                None,
                &state.completed_fanout_children,
                state.held_response_release.as_deref(),
                Some(&state.tool_results),
                Some(&state.guidance_pending),
                Some(&state.guidance_requests),
            )
            .await
        }
        MockScenario::FanoutPartialThenComplete => {
            body_fanout_then_complete_with_callbacks(
                &request_body,
                Some(1),
                &state.completed_fanout_children,
                state.held_response_release.as_deref(),
                Some(&state.tool_results),
                Some(&state.guidance_pending),
                Some(&state.guidance_requests),
            )
            .await
        }
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
        if records.len() == MAX_RECORDED_TOOL_RESULTS {
            records.remove(0);
        }
        records.push(serde_json::to_value(&record).expect("tool result request serializes"));
    }
    Ok(axum::Json(serde_json::json!({"accepted": true})))
}

async fn handle_run_user_intent(
    State(state): State<ServerState>,
    Path(run_id): Path<String>,
    body: axum::body::Bytes,
) -> Result<axum::Json<astra_thin_client::RunUserIntentResponse>, (StatusCode, axum::Json<Value>)> {
    let request = serde_json::from_slice::<astra_thin_client::RunUserIntentRequest>(&body)
        .map_err(|error| {
            (
                StatusCode::BAD_REQUEST,
                axum::Json(serde_json::json!({
                    "accepted": false,
                    "error": format!("invalid run intent envelope: {error}"),
                })),
            )
        })?;
    let expected_run = match state.scenario {
        MockScenario::FanoutThenComplete | MockScenario::FanoutPartialThenComplete => {
            Some("mock-run-fanout-root")
        }
        _ => None,
    };
    let input_is_typed = request
        .input
        .get("content")
        .and_then(Value::as_str)
        .is_some_and(|content| !content.trim().is_empty())
        && request.input["astra_runtime_context"]["authority"] == "run_control_provider"
        && request.input["astra_runtime_context"]["schema"] == "active_work_snapshot.v1";
    if expected_run != Some(run_id.as_str())
        || request.delivery != astra_turn_types::UserIntentDelivery::GuideCurrentRun
        || !input_is_typed
    {
        return Err((
            StatusCode::BAD_REQUEST,
            axum::Json(serde_json::json!({
                "accepted": false,
                "error": "run intent identity, delivery, or typed context is invalid",
            })),
        ));
    }
    if let Ok(mut requests) = state.guidance_requests.lock() {
        const MAX_RECORDED_GUIDANCE_REQUESTS: usize = 16;
        if requests.len() == MAX_RECORDED_GUIDANCE_REQUESTS {
            requests.remove(0);
        }
        requests.push(serde_json::to_value(&request).expect("run intent request serializes"));
    }
    state.guidance_pending.store(1, Ordering::Release);
    Ok(axum::Json(astra_thin_client::RunUserIntentResponse {
        run_id,
        intent_id: request.intent_id,
        status: astra_turn_types::UserIntentStatus::AcceptedRemote,
        duplicate: false,
        event_index: 0,
    }))
}

async fn handle_run_stream(
    State(state): State<ServerState>,
    Path(run_id): Path<String>,
) -> Response<axum::body::Body> {
    if run_id != "mock-run-fanout-root" {
        return (StatusCode::NOT_FOUND, "unknown mock run").into_response();
    }
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(15);
    while state.guidance_pending.load(Ordering::Acquire) != 2 {
        if tokio::time::Instant::now() >= deadline {
            return (StatusCode::NO_CONTENT, "").into_response();
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let request = state
        .guidance_requests
        .lock()
        .ok()
        .and_then(|requests| requests.last().cloned());
    let Some(request) = request else {
        return (StatusCode::CONFLICT, "missing mock guidance request").into_response();
    };
    (
        [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
        user_intent_applied_event(&run_id, &request, 1),
    )
        .into_response()
}

// ─── Public API ───────────────────────────────────────────────────────────────

/// A running mock LLM server. Drop to shut down.
pub struct MockLlmServer {
    pub base_url: String,
    received_requests: Arc<Mutex<Vec<Value>>>,
    tool_results: Arc<Mutex<Vec<Value>>>,
    guidance_requests: Arc<Mutex<Vec<Value>>>,
    completed_fanout_children: Arc<AtomicU8>,
    held_response_release: Option<Arc<tokio::sync::Notify>>,
    _shutdown: tokio::sync::oneshot::Sender<()>,
}

impl MockLlmServer {
    /// Start the mock server on a random free port. Returns immediately.
    pub async fn start(scenario: MockScenario) -> Result<Self, String> {
        Self::start_inner(scenario, false).await
    }

    /// Start a fanout fixture whose final child remains live until explicitly
    /// released. This gives lifecycle journeys an authoritative concurrency
    /// boundary instead of racing a wall-clock delay.
    pub async fn start_with_held_fanout_child(scenario: MockScenario) -> Result<Self, String> {
        if !matches!(
            scenario,
            MockScenario::FanoutThenComplete | MockScenario::FanoutPartialThenComplete
        ) {
            return Err("held fanout child requires a fanout scenario".to_string());
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
        let guidance_requests = Arc::new(Mutex::new(Vec::new()));
        let guidance_pending = Arc::new(AtomicU8::new(0));
        let completed_fanout_children = Arc::new(AtomicU8::new(0));
        let held_response_release = hold_response.then(|| Arc::new(tokio::sync::Notify::new()));
        let state = ServerState {
            scenario,
            call_count: Arc::new(AtomicU32::new(0)),
            received_requests: received_requests.clone(),
            issued_tool_requests,
            tool_results: tool_results.clone(),
            guidance_requests: guidance_requests.clone(),
            guidance_pending,
            completed_fanout_children: completed_fanout_children.clone(),
            held_response_release: held_response_release.clone(),
        };

        let app = Router::new()
            .route("/chat/stream", post(handle_chat_turn))
            .route("/tools/result", post(handle_tool_result))
            .route("/chat/runs/{run_id}/intents", post(handle_run_user_intent))
            .route("/chat/runs/{run_id}/stream", get(handle_run_stream))
            .route("/sessions", post(handle_create_session))
            .route("/models", get(handle_models))
            .route("/model-access", get(handle_model_access))
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
            guidance_requests,
            completed_fanout_children,
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

    pub fn completed_fanout_children(&self) -> u32 {
        self.completed_fanout_children
            .load(Ordering::Acquire)
            .count_ones()
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

    /// Return the bounded active-run guidance requests accepted by the mock.
    pub fn guidance_requests(&self) -> Vec<Value> {
        self.guidance_requests
            .lock()
            .map(|requests| requests.clone())
            .unwrap_or_default()
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
        IssuedToolRequestIdentity, MockScenario, body_agent_then_complete, body_complete,
        body_fail, body_malformed_json, body_multi_turn, body_rate_limited, body_sse_chunk_split,
        body_text_only, body_tool_then_complete, callback_recorded, issued_tool_requests_from_sse,
        root_has_tool_result, tool_request_for_run,
    };
    use serde_json::Value;
    use std::sync::{Arc, Mutex};

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
    fn tool_body_models_a_real_two_turn_client_side_tool_protocol() {
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
    fn tool_result_identity_survives_pre_resolved_transport_names() {
        let request = serde_json::json!({
            "messages": [
                {
                    "role": "assistant",
                    "tool_calls": [{
                        "id": "call-agent-1",
                        "type": "function",
                        "function": {"name": "agent", "arguments": "{}"}
                    }]
                },
                {
                    "role": "tool",
                    "tool_call_id": "call-agent-1",
                    "_tool_name": "pre_resolved",
                    "content": "result"
                }
            ],
            "tool_results": [{
                "tool_call_id": "call-agent-1",
                "name": "pre_resolved",
                "result": "result"
            }]
        });

        assert!(root_has_tool_result(&request, "agent"));
        assert!(!root_has_tool_result(&request, "tool_search"));
    }

    #[test]
    fn callback_progress_requires_a_completed_typed_result() {
        let callbacks = Arc::new(Mutex::new(vec![
            serde_json::json!({"request_id": "failed", "status": "failed"}),
            serde_json::json!({"request_id": "complete", "status": "completed"}),
            serde_json::json!({"request_id": "missing-status"}),
        ]));

        assert!(!callback_recorded(&callbacks, "failed"));
        assert!(callback_recorded(&callbacks, "complete"));
        assert!(!callback_recorded(&callbacks, "missing-status"));
    }

    #[tokio::test]
    async fn agent_then_complete_spawn_round_emits_executable_tool_request() {
        let request = serde_json::json!({
            "messages": [
                {
                    "role": "assistant",
                    "tool_calls": [{
                        "id": "call-activate-agent",
                        "type": "function",
                        "function": {"name": "tool_search", "arguments": "{}"}
                    }]
                },
                {
                    "role": "tool",
                    "tool_call_id": "call-activate-agent",
                    "content": "selected agent"
                }
            ],
            "tool_results": [{
                "tool_call_id": "call-activate-agent",
                "name": "tool_search",
                "result": "selected agent"
            }]
        });
        let body = body_agent_then_complete(&request).await;
        let mut accum = astra_turn_core::chat_turn_sse_dispatch::ChatTurnSseAccum::default();
        let mut pending = Vec::new();

        for event in parse_sse_events(&body) {
            astra_turn_core::chat_turn_sse_dispatch::dispatch_chat_turn_sse_event_block(
                &format!("data: {event}\n\n"),
                &mut accum,
                &mut pending,
            );
        }

        let agent_request = pending
            .iter()
            .find_map(|item| match item {
                astra_turn_core::chat_turn_sse_dispatch::ChatTurnEdgePending::ToolRequest {
                    request_id,
                    tool,
                    args,
                    ..
                } if tool == "agent" => Some((request_id, args)),
                _ => None,
            })
            .expect("spawn round must expose an executable agent tool_request");
        assert_eq!(agent_request.0, "call-spawn-child");
        assert_eq!(agent_request.1["action"], "spawn");
        assert_eq!(agent_request.1["prompt"], super::AGENT_JOURNEY_CHILD_TASK);
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
        // Yet must still have a complete turn shape.
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
