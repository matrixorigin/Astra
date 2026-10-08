//! Phase B: `/chat/stream` (server-driven loop) persistence — verify session,
//! core events (`user_query`, `llm_response`), context trace events, and run
//! status in DB after web-agent mode chat.
//!
//! ## Architecture note
//!
//! The `/chat/stream` path uses the Server-owned agentic loop. Canonical append
//! and atomic terminal settlement persist user/assistant transcript items,
//! accepted trace events, run status, and usage under the execution owner fence.
//! Context trace and promotion events retain their existing publication owners.
//!
//! These tests verify that infrastructure.

use super::harness::{ProviderResponse, ProviderScript};
use axum::http::StatusCode;
use axum::{body::Body, http::Request};
use futures_util::StreamExt;
use serde_json::{Value, json};
use sqlx::Row;
use tower::util::ServiceExt;
use uuid::Uuid;

use super::harness::{
    E2E_PASSWORD, bootstrap, cleanup_session_data, delete_json, get_json, parse_sse_events,
    post_json, seeded_model_selection, sse_first_data_json_with_type,
    try_claim_interrupted_matrix_e2e_fixture,
};

/// Collect the FULL SSE stream body (up to deadline), not just until session_info.
/// Returns (status, body_text).
pub(super) async fn collect_full_sse_stream(
    app: &axum::Router,
    req: Request<Body>,
    timeout_secs: u64,
) -> (StatusCode, String) {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);
    let resp = tokio::time::timeout_at(deadline, app.clone().oneshot(req))
        .await
        .unwrap_or_else(|_| panic!("SSE request did not return headers within {timeout_secs}s"))
        .expect("oneshot");
    let status = resp.status();
    let mut stream = resp.into_body().into_data_stream();
    let mut acc = Vec::new();
    loop {
        match tokio::time::timeout_at(deadline, stream.next()).await {
            Ok(Some(chunk)) => {
                let chunk = chunk.expect("body chunk");
                acc.extend_from_slice(&chunk);
            }
            Ok(None) => break,
            Err(_) => {
                let events = parse_sse_events(&String::from_utf8_lossy(&acc));
                let recent = events
                    .iter()
                    .rev()
                    .take(8)
                    .map(|event| (event["type"].as_str(), event["status"].as_str()))
                    .collect::<Vec<_>>();
                panic!(
                    "SSE stream did not terminate within {timeout_secs}s; collected {} bytes; recent event type/status (newest first): {recent:?}",
                    acc.len()
                );
            }
        }
    }
    (status, String::from_utf8_lossy(&acc).into_owned())
}

/// Stream a chat, wait for the full stream to end, return (status, raw_body).
async fn stream_chat_full(app: &axum::Router, auth: &str, payload: Value) -> (StatusCode, String) {
    stream_chat_full_with_timeout(app, auth, payload, 30).await
}

async fn stream_chat_full_with_timeout(
    app: &axum::Router,
    auth: &str,
    payload: Value,
    timeout_secs: u64,
) -> (StatusCode, String) {
    let test_secret = std::env::var("ASTRA_TEST_E2E_SECRET").expect("bridge test secret");
    let req = Request::builder()
        .method("POST")
        .uri("/chat/stream")
        .header("authorization", auth)
        .header("content-type", "application/json")
        .header("x-astra-e2e-test-secret", &test_secret)
        .body(Body::from(payload.to_string()))
        .expect("stream request");
    collect_full_sse_stream(app, req, timeout_secs).await
}

#[tokio::test(start_paused = true)]
#[should_panic(expected = "SSE request did not return headers within 5s")]
async fn sse_deadline_covers_pending_request_headers() {
    let app = axum::Router::new().route("/", axum::routing::get(std::future::pending::<String>));
    collect_full_sse_stream(&app, Request::new(Body::empty()), 5).await;
}

#[tokio::test(start_paused = true)]
#[should_panic(expected = "SSE stream did not terminate within 5s")]
async fn sse_deadline_is_shared_by_headers_and_body() {
    let app = axum::Router::new().route(
        "/",
        axum::routing::get(|| async {
            tokio::time::sleep(std::time::Duration::from_secs(3)).await;
            Body::from_stream(futures_util::stream::once(async {
                tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                Ok::<_, std::convert::Infallible>("data: {\"type\":\"done\"}\n\n")
            }))
        }),
    );
    collect_full_sse_stream(&app, Request::new(Body::empty()), 5).await;
}

#[tokio::test(start_paused = true)]
async fn sse_deadline_accepts_a_complete_stream() {
    let expected = "data: {\"type\":\"done\"}\n\n";
    let app = axum::Router::new().route("/", axum::routing::get(move || async move { expected }));
    let (status, body) = collect_full_sse_stream(&app, Request::new(Body::empty()), 5).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, expected);
}

fn delegation_assessment(body: &Value) -> Option<Value> {
    body["messages"].as_array()?.iter().find_map(|message| {
        let input: Value = serde_json::from_str(message["content"].as_str()?).ok()?;
        (input["user_text"].is_string()
            && input["candidates"].is_array()
            && input["slots"].is_array())
        .then_some(input)
    })
}

// A dropped observer must never strand a gated provider fixture.
struct ReleaseProviderGatesOnDrop(Vec<std::sync::Arc<tokio::sync::Notify>>);
impl Drop for ReleaseProviderGatesOnDrop {
    fn drop(&mut self) {
        for gate in &self.0 {
            gate.notify_one();
        }
    }
}

fn gated_native_delta(
    delta: Value,
    finish: &str,
    gate: std::sync::Arc<tokio::sync::Notify>,
) -> ProviderResponse {
    ProviderResponse::Stream {
        content_type: "text/event-stream",
        chunks: vec![
            format!(
                "data: {}\n\n",
                json!({"choices":[{"index":0,"delta":delta}]})
            )
            .into_bytes(),
            format!(
                "data: {}\n\n",
                json!({"choices":[{"index":0,"delta":{},"finish_reason":finish}],"usage":{"prompt_tokens":42,"completion_tokens":7,"total_tokens":49}})
            )
            .into_bytes(),
            b"data: [DONE]\n\n".to_vec(),
        ],
        release_before_chunk: Some((0, gate)),
    }
}

fn native_text_response(text: &str) -> ProviderResponse {
    ProviderResponse::OpenAi(
        json!({"choices":[{"index":0,"message":{"role":"assistant","content":text},"finish_reason":"stop"}],"usage":{"prompt_tokens":42,"completion_tokens":7,"total_tokens":49}}),
    )
}

fn native_tool_response(call: Value) -> ProviderResponse {
    ProviderResponse::OpenAi(
        json!({"choices":[{"index":0,"message":{"role":"assistant","content":"","tool_calls":[call]},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":42,"completion_tokens":7,"total_tokens":49}}),
    )
}

fn native_search_response(id: &str, tool: &str) -> ProviderResponse {
    native_tool_response(provider_tool_call(
        id,
        "tool_search",
        json!({"query":format!("select:{tool}")}),
    ))
}

fn native_invoke_response(id: &str, tool: &str, arguments: Value) -> ProviderResponse {
    native_tool_response(provider_tool_call(
        id,
        "invoke_tool",
        json!({"name":tool,"arguments":arguments}),
    ))
}

fn native_child_script(
    model: String,
    root_message: &'static str,
    task: &'static str,
    response: ProviderResponse,
) -> ProviderScript {
    ProviderScript::new(
        format!("exact delegated task: {task}"),
        move |request| {
            request.path == "/v1/chat/completions"
                && request.body["model"] == model
                && request.body["stream"] == true
                && delegation_assessment(&request.body).is_none()
                && request.body["messages"].as_array().is_some_and(|messages| {
                    messages
                        .iter()
                        .any(|message| message["role"] == "user" && message["content"] == task)
                })
                && !request.body["messages"].as_array().is_some_and(|messages| {
                    messages.iter().any(|message| {
                        message["role"] == "user" && message["content"] == root_message
                    })
                })
        },
        vec![response],
    )
}

async fn assert_native_delegation_without_auxiliary_selector(
    ctx: &super::harness::MatrixE2eCtx,
    user_text: &str,
    slots: &[(&str, &str)],
) {
    let requests = ctx.native_provider_requests().await;
    assert_eq!(
        requests
            .iter()
            .filter(|request| delegation_assessment(&request.body).is_some())
            .count(),
        0,
        "ordinary delegation must not make an auxiliary selector request"
    );
    let request = requests
        .iter()
        .rfind(|request| {
            request.body["messages"].as_array().is_some_and(|messages| {
                messages
                    .iter()
                    .any(|message| message["role"] == "user" && message["content"] == user_text)
            })
        })
        .expect("parent must reach the provider");
    assert_eq!(request.body["model"], format!("mock-{}", ctx.suffix));
    assert_eq!(request.body["stream"], true);
    let mut actual = Vec::new();
    for call in request.body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|message| message["role"] == "assistant")
        .filter_map(|message| message["tool_calls"].as_array())
        .flatten()
    {
        let name = call["function"]["name"].as_str().unwrap();
        let args: Value =
            serde_json::from_str(call["function"]["arguments"].as_str().unwrap()).unwrap();
        let (name, args) = if name == "invoke_tool" {
            (args["name"].as_str().unwrap(), &args["arguments"])
        } else {
            (name, &args)
        };
        match (name, args["action"].as_str()) {
            ("agent", Some("spawn")) => actual.push(args.clone()),
            ("agent_fanout", Some("start")) => {
                actual.extend(args["slots"].as_array().unwrap().iter().cloned())
            }
            _ => {}
        }
    }
    assert_eq!(actual.len(), slots.len());
    for (index, (description, prompt)) in slots.iter().enumerate() {
        assert_eq!(actual[index]["description"], *description);
        assert_eq!(actual[index]["prompt"], *prompt);
        assert_eq!(
            requests
                .iter()
                .filter(
                    |request| request.body["model"] == format!("mock-{}", ctx.suffix)
                        && request.body["stream"] == true
                        && delegation_assessment(&request.body).is_none()
                        && request.body["messages"]
                            .as_array()
                            .is_some_and(|messages| messages
                                .iter()
                                .any(|message| message["role"] == "user"
                                    && message["content"] == *prompt))
                )
                .count(),
            1,
            "each declared slot must issue exactly one actual child request with its complete task"
        );
    }
}

fn provider_tool_call(id: &str, name: &str, args: Value) -> Value {
    json!({
        "id": id,
        "type": "function",
        "function": {
            "name": name,
            "arguments": args.to_string()
        }
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OnlineFanoutTerminal {
    Completed,
    Failed,
}

impl OnlineFanoutTerminal {
    fn child_event_type(self) -> &'static str {
        match self {
            Self::Completed => "agent_completed",
            Self::Failed => "agent_failed",
        }
    }

    fn child_run_status(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Failed => "failed",
        }
    }
}

#[derive(Clone, Debug)]
struct ConcurrentFanoutCase {
    name: String,
    auth: String,
    user_id: String,
    session_id: String,
    terminal: OnlineFanoutTerminal,
    final_reply: String,
    child_provenance: String,
}

#[derive(Debug)]
struct FanoutStreamEvidence {
    root_run_id: String,
    child_run_ids: Vec<String>,
}

fn concurrent_fanout_scripts(
    ctx: &super::harness::MatrixE2eCtx,
    case: &ConcurrentFanoutCase,
    shared_group_id: &str,
) -> Vec<ProviderScript> {
    let message = format!("Run the isolated fanout scenario {}.", case.name);
    let root_message = message.clone();
    let root_model = format!("mock-{}", ctx.suffix);
    let child_model = root_model.clone();
    let child_anchor = format!("{}: Return the", case.name);
    let call_id = format!("{}-fanout-start", case.name);
    let responses = vec![
        native_search_response(&format!("{}-tool-search", case.name), "agent_fanout"),
        native_invoke_response(
            &call_id,
            "agent_fanout",
            json!({
                "action": "start",
                // Intentional collision: a fanout group is scoped
                // by owner + session + parent, never by this label.
                "group_id": shared_group_id,
                "title": "Concurrent isolation gate",
                "target_count": 2,
                "slots": [
                    {
                        "id": "first",
                        "description": "First isolated child",
                        "prompt": format!("{}: Return the first independent finding.",case.name)
                    },
                    {
                        "id": "second",
                        "description": "Second isolated child",
                        "prompt": format!("{}: Return the second independent finding.",case.name)
                    }
                ],
                "defaults": {"agent_type": "general-purpose"}
            }),
        ),
        native_invoke_response(
            "isolated-fanout-wait",
            "agent",
            json!({"action": "wait", "timeout_ms": 10000}),
        ),
        native_invoke_response(
            &format!("{}-fanout-results", case.name),
            "agent_fanout",
            json!({"action": "get_results", "group_id": shared_group_id}),
        ),
        native_text_response(&case.final_reply),
    ];
    let child_response = || match case.terminal {
        OnlineFanoutTerminal::Completed => native_text_response(&case.child_provenance),
        OnlineFanoutTerminal::Failed => ProviderResponse::Json {
            status: StatusCode::BAD_REQUEST,
            body: json!({"error":{"message":case.child_provenance}}),
        },
    };
    vec![
        ProviderScript::new(
            format!("{} exact root", case.name),
            move |request| {
                request.path == "/v1/chat/completions"
                    && request.body["model"] == root_model
                    && request.body["stream"] == true
                    && delegation_assessment(&request.body).is_none()
                    && request.body["messages"].as_array().is_some_and(|messages| {
                        messages
                            .iter()
                            .any(|m| m["role"] == "user" && m["content"] == root_message)
                    })
            },
            responses,
        ),
        ProviderScript::new(
            format!("{} exact children", case.name),
            move |request| {
                request.path == "/v1/chat/completions"
                    && request.body["model"] == child_model
                    && request.body["stream"] == true
                    && delegation_assessment(&request.body).is_none()
                    && request.body["messages"].as_array().is_some_and(|messages| {
                        messages.iter().any(|m| {
                            m["content"]
                                .as_str()
                                .is_some_and(|text| text.contains(&child_anchor))
                        })
                    })
                    && !request.body["messages"].as_array().is_some_and(|messages| {
                        messages
                            .iter()
                            .any(|m| m["role"] == "user" && m["content"] == message)
                    })
            },
            vec![child_response(), child_response()],
        ),
    ]
}

fn concurrent_fanout_payload(
    ctx: &super::harness::MatrixE2eCtx,
    case: &ConcurrentFanoutCase,
) -> Value {
    json!({"message":format!("Run the isolated fanout scenario {}.",case.name),"session_id":case.session_id,"model_selection":seeded_model_selection(ctx),"execution_policy":{"turn_intent":"fixed_default","skill_auto_route":"disabled"}})
}

async fn captured_synthesis_children(
    pool: &sqlx::MySqlPool,
    user_id: &str,
    session_id: &str,
    root_run_id: &str,
    final_reply: &str,
    events: &[Value],
) -> Vec<Value> {
    let completed_rounds = events
        .iter()
        .find(|event| event["type"] == "turn_complete")
        .and_then(|event| event["llm_rounds"].as_u64())
        .and_then(|rounds| rounds.checked_sub(1))
        .expect("accepted final provider round");
    let captures = sqlx::query_scalar::<_, String>(
        "SELECT content_json FROM session_artifacts \
         WHERE user_id = ? AND session_id = ? AND turn = 1 \
           AND artifact_kind = 'llm_capture' AND source = 'server_loop_host' \
           AND round = ? LIMIT 2",
    )
    .bind(user_id)
    .bind(session_id)
    .bind(completed_rounds)
    .fetch_all(pool)
    .await
    .expect("actual provider exchanges");
    assert_eq!(
        captures.len(),
        1,
        "one capture for the accepted final round"
    );
    let exchange: Value = serde_json::from_str(&captures[0]).unwrap();
    assert_eq!(exchange["response"]["full_text"], final_reply);
    assert!(
        exchange["response"]["tool_calls"]
            .as_array()
            .is_none_or(Vec::is_empty)
    );
    let mut children = Vec::new();
    for message in exchange["request"]["messages"].as_array().unwrap() {
        let Some(payload) = message["content"]
            .as_str()
            .and_then(|content| content.strip_prefix("<runtime-required-context>\n"))
            .and_then(|content| content.strip_suffix("\n</runtime-required-context>"))
            .and_then(|content| serde_json::from_str::<Value>(content).ok())
        else {
            continue;
        };
        if payload["context"]["schema"] != "direct_child_completion.v1" {
            continue;
        }
        assert_eq!(payload["kind"], "background_task_notification");
        let payload = &payload["context"];
        assert_eq!(payload["parent_run_id"], root_run_id);
        children.extend(payload["children"].as_array().unwrap().iter().cloned());
    }
    for child in &children {
        let result = child["result"]
            .as_str()
            .expect("authoritative child payload");
        assert_eq!(child["result_truncated"], false);
        assert_eq!(child["result_bytes"], result.len());
    }
    children
}

async fn assert_concurrent_fanout_stream(
    pool: &sqlx::MySqlPool,
    case: &ConcurrentFanoutCase,
    shared_group_id: &str,
    all_child_provenances: &[String],
    raw_sse: &str,
) -> FanoutStreamEvidence {
    let events = parse_sse_events(raw_sse);
    let root_run_id = events
        .iter()
        .find(|event| event["type"].as_str() == Some("session_info"))
        .and_then(|event| event["run_id"].as_str())
        .unwrap_or_else(|| panic!("{}: missing root run id: {raw_sse}", case.name))
        .to_string();
    let terminal_positions = events
        .iter()
        .enumerate()
        .filter(|(_, event)| {
            event["type"].as_str() == Some(case.terminal.child_event_type())
                || event["event_type"].as_str() == Some(case.terminal.child_event_type())
        })
        .map(|(index, _)| index)
        .collect::<Vec<_>>();
    assert_eq!(
        terminal_positions.len(),
        2,
        "{}: every declared slot must emit exactly one child terminal: {raw_sse}",
        case.name
    );
    let final_positions = events
        .iter()
        .enumerate()
        .filter(|(_, event)| {
            event["type"].as_str() == Some("text_delta")
                && event["content"].as_str() == Some(case.final_reply.as_str())
        })
        .map(|(index, _)| index)
        .collect::<Vec<_>>();
    assert_eq!(
        final_positions.len(),
        1,
        "{}: the parent must synthesize exactly once: {raw_sse}",
        case.name
    );
    assert!(
        terminal_positions
            .iter()
            .all(|position| *position < final_positions[0]),
        "{}: parent synthesis preceded a child terminal: {raw_sse}",
        case.name
    );
    let terminal_events = events
        .iter()
        .filter(|event| event["type"].as_str() == Some("turn_complete"))
        .collect::<Vec<_>>();
    assert_eq!(
        terminal_events.len(),
        1,
        "{}: one user turn must expose one terminal boundary",
        case.name
    );
    let root_terminal = events.iter().find(|event| {
        event["type"] == "run_finished" && event["run_id"].as_str() == Some(root_run_id.as_str())
    });
    assert_eq!(
        root_terminal.and_then(|event| event["status"].as_str()),
        Some("completed"),
        "{}: parent synthesis must finish successfully, including when children fail: {root_terminal:?}",
        case.name,
    );

    let call_id = format!("{}-fanout-start", case.name);
    let aggregate = events
        .iter()
        .find(|event| {
            event["type"].as_str() == Some("tool_call_end")
                && event["call_id"].as_str() == Some(call_id.as_str())
        })
        .and_then(|event| event["result"].as_str())
        .and_then(|result| serde_json::from_str::<Value>(result).ok())
        .unwrap_or_else(|| panic!("{}: missing canonical fanout result: {raw_sse}", case.name));
    assert_eq!(aggregate["group_id"].as_str(), Some(shared_group_id));
    assert_eq!(aggregate["target_count"].as_u64(), Some(2));
    assert_eq!(aggregate["status"], "started");
    let launched = aggregate["agents"]
        .as_array()
        .expect("accepted launch identities");
    assert_eq!(launched.len(), 2);
    assert_eq!(
        launched
            .iter()
            .map(|child| child["slot_index"].as_u64().unwrap())
            .collect::<Vec<_>>(),
        vec![0, 1]
    );
    let results = captured_synthesis_children(
        pool,
        &case.user_id,
        &case.session_id,
        &root_run_id,
        &case.final_reply,
        &events,
    )
    .await;
    assert_eq!(results.len(), 2, "{}: {results:?}", case.name);
    assert!(
        results
            .iter()
            .all(|result| result["status"] == case.terminal.child_run_status())
    );
    assert_eq!(
        results
            .iter()
            .map(|result| (
                result["agent_id"].as_str().unwrap(),
                result["run_id"].as_str().unwrap()
            ))
            .collect::<std::collections::HashSet<_>>(),
        launched
            .iter()
            .map(|child| (
                child["agent_id"].as_str().unwrap(),
                child["run_id"].as_str().unwrap()
            ))
            .collect::<std::collections::HashSet<_>>()
    );
    assert!(
        results.iter().all(|result| {
            let payload = result["result"].as_str().unwrap();
            if case.terminal == OnlineFanoutTerminal::Completed {
                payload == case.child_provenance
            } else {
                payload.contains("[invalid_request] LLM request rejected: 400")
            }
        }),
        "{}: every fixed slot must retain its own payload provenance: {aggregate}",
        case.name
    );
    if case.terminal == OnlineFanoutTerminal::Failed {
        assert!(!raw_sse.contains(&case.child_provenance));
        assert!(!json!(results).to_string().contains(&case.child_provenance));
    }
    let aggregate_text = json!(results).to_string();
    assert!(
        all_child_provenances
            .iter()
            .filter(|provenance| *provenance != &case.child_provenance)
            .all(|provenance| !aggregate_text.contains(provenance)),
        "{}: foreign user/session payload leaked into its fanout aggregate: {aggregate}",
        case.name
    );
    let child_run_ids = launched
        .iter()
        .map(|result| {
            result["run_id"]
                .as_str()
                .unwrap_or_else(|| panic!("{}: child run identity missing: {aggregate}", case.name))
                .to_string()
        })
        .collect::<Vec<_>>();
    assert_ne!(
        child_run_ids[0], child_run_ids[1],
        "{}: each fixed slot owns one distinct run",
        case.name
    );
    FanoutStreamEvidence {
        root_run_id,
        child_run_ids,
    }
}

async fn assert_concurrent_fanout_durability(
    pool: &sqlx::MySqlPool,
    case: &ConcurrentFanoutCase,
    evidence: &FanoutStreamEvidence,
) {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(20);
    let rows = loop {
        let rows = sqlx::query(
            "SELECT run_id, user_id, session_id, parent_run_id, root_run_id, depth, status \
             FROM agent_runs WHERE user_id = ? AND session_id = ? \
             ORDER BY depth ASC, run_id ASC",
        )
        .bind(&case.user_id)
        .bind(&case.session_id)
        .fetch_all(pool)
        .await
        .expect("load isolated durable fanout tree");
        if rows.len() == 3
            && rows.iter().all(|row| {
                matches!(
                    row.get::<String, _>("status").as_str(),
                    "completed" | "failed"
                )
            })
        {
            break rows;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "{}: fanout tree did not settle to exactly three rows: {rows:?}",
            case.name
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    };
    assert_eq!(rows.len(), 3, "{}: {rows:?}", case.name);
    assert!(rows.iter().all(|row| {
        row.get::<String, _>("user_id") == case.user_id
            && row.get::<String, _>("session_id") == case.session_id
    }));
    let root = rows
        .iter()
        .find(|row| row.get::<i32, _>("depth") == 0)
        .unwrap_or_else(|| panic!("{}: missing root row: {rows:?}", case.name));
    assert_eq!(
        root.get::<String, _>("run_id"),
        evidence.root_run_id,
        "{}: root identity drifted",
        case.name
    );
    assert_eq!(root.get::<String, _>("status"), "completed");
    let children = rows
        .iter()
        .filter(|row| row.get::<i32, _>("depth") == 1)
        .collect::<Vec<_>>();
    assert_eq!(children.len(), 2, "{}: {rows:?}", case.name);
    assert!(children.iter().all(|row| {
        row.get::<Option<String>, _>("parent_run_id").as_deref()
            == Some(evidence.root_run_id.as_str())
            && row.get::<String, _>("root_run_id") == evidence.root_run_id
            && row.get::<String, _>("status") == case.terminal.child_run_status()
    }));
    let mut durable_child_run_ids = children
        .iter()
        .map(|row| row.get::<String, _>("run_id"))
        .collect::<Vec<_>>();
    let mut streamed_child_run_ids = evidence.child_run_ids.clone();
    durable_child_run_ids.sort();
    streamed_child_run_ids.sort();
    assert_eq!(
        durable_child_run_ids, streamed_child_run_ids,
        "{}: stream and durable child identities diverged",
        case.name
    );

    let orphan_transcripts: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM session_transcript_items AS transcript \
         LEFT JOIN agent_runs AS run \
           ON run.user_id = transcript.user_id \
          AND run.session_id = transcript.session_id \
          AND run.run_id = transcript.run_id \
         WHERE transcript.user_id = ? AND transcript.session_id = ? \
           AND transcript.run_id IS NOT NULL AND run.run_id IS NULL",
    )
    .bind(&case.user_id)
    .bind(&case.session_id)
    .fetch_one(pool)
    .await
    .expect("count isolated orphan transcript identities");
    assert_eq!(orphan_transcripts, 0, "{}: orphan transcript", case.name);
    let reconciliations: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM agent_events WHERE user_id = ? AND session_id = ? \
         AND event_type = 'runtime_reconciliation'",
    )
    .bind(&case.user_id)
    .bind(&case.session_id)
    .fetch_one(pool)
    .await
    .expect("count isolated runtime reconciliations");
    assert_eq!(
        reconciliations, 0,
        "{}: foreground fanout escaped its original root turn",
        case.name
    );
}

/// Poll until `agent_events` has at least `min_count` rows for the session.
async fn wait_for_agent_events_count(
    pool: &sqlx::MySqlPool,
    user_id: &str,
    session_id: &str,
    min_count: i64,
    timeout: std::time::Duration,
) {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let n: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM agent_events WHERE user_id = ? AND session_id = ?",
        )
        .bind(user_id)
        .bind(session_id)
        .fetch_one(pool)
        .await
        .unwrap_or(0);
        if n >= min_count {
            return;
        }
        if tokio::time::Instant::now() >= deadline {
            panic!(
                "timeout ({timeout:?}) waiting for >= {min_count} agent_events for user_id={user_id} session_id={session_id} (got {n})"
            );
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}

/// Starting another in-process app must not interpret an already-live Matrix
/// fixture as interrupted state. The second bootstrap crosses the same stale
/// cleanup path used after developer interrupts, then the first app proves its
/// session and mock Offering still form an executable user turn.
pub async fn run_stream_bootstrap_cleanup_preserves_live_fixture() {
    let first = bootstrap().await;
    let first_user_id = first.ctx.user_id.clone();
    let first_session_id = first.ctx.session_id.clone();
    let first_offering_id = first.ctx.model_offering_id.clone();

    // Model a long-running fixture: creation time alone is stale, while its
    // durable heartbeat remains fresh. A cleanup policy based only on age
    // would still destroy this active user in a parallel process.
    sqlx::query(
        "UPDATE auth_users
         SET created_at = DATE_SUB(CURRENT_TIMESTAMP(), INTERVAL 1 DAY),
             last_login_at = CURRENT_TIMESTAMP(6)
         WHERE user_id = ?",
    )
    .bind(&first_user_id)
    .execute(&first.ctx.pool)
    .await
    .expect("backdate live fixture while retaining its heartbeat");

    let stale = bootstrap().await;
    let stale_session_id = stale.ctx.session_id.clone();
    let stale_offering_id = stale.ctx.model_offering_id.clone();
    stale.ctx.expire_fixture_lease_for_test().await;
    // Reproduce the cross-process race at the linearization point: a cleaner
    // has already selected this expired candidate, then its owner renews
    // before the conditional claim. The stale snapshot must lose.
    sqlx::query("UPDATE auth_users SET last_login_at = CURRENT_TIMESTAMP(6) WHERE user_id = ?")
        .bind(&stale.ctx.user_id)
        .execute(&stale.ctx.pool)
        .await
        .expect("renew fixture after cleaner candidate selection");
    assert!(
        !try_claim_interrupted_matrix_e2e_fixture(&stale.ctx.shared_pool, &stale.ctx.user_id).await,
        "an owner renewal after candidate selection must defeat the cleanup claim"
    );
    stale.ctx.expire_fixture_lease_for_test().await;

    let second = bootstrap().await;

    let session_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM agent_sessions WHERE user_id = ? AND session_id = ?",
    )
    .bind(&first_user_id)
    .bind(&first_session_id)
    .fetch_one(&first.ctx.pool)
    .await
    .expect("count first live session after second bootstrap");
    assert_eq!(
        session_count, 1,
        "a later bootstrap must not reclaim an active fixture session"
    );
    let offering_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM infra_llm_models WHERE model_id = ?")
            .bind(&first_offering_id)
            .fetch_one(&first.ctx.pool)
            .await
            .expect("count first live Offering after second bootstrap");
    assert_eq!(
        offering_count, 1,
        "a later bootstrap must not reclaim an active fixture Offering"
    );
    let stale_session_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM agent_sessions WHERE session_id = ?")
            .bind(&stale_session_id)
            .fetch_one(&first.ctx.pool)
            .await
            .expect("count stale session after cleanup");
    assert_eq!(
        stale_session_count, 0,
        "a later bootstrap must reclaim an expired fixture session"
    );
    let stale_offering_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM infra_llm_models WHERE model_id = ?")
            .bind(&stale_offering_id)
            .fetch_one(&first.ctx.pool)
            .await
            .expect("count stale Offering after cleanup");
    assert_eq!(
        stale_offering_count, 0,
        "a later bootstrap must reclaim an expired fixture Offering"
    );

    let final_reply = "The first live fixture remains executable after a second bootstrap.";
    let fixture_model = format!("mock-{}", first.ctx.suffix);
    first.ctx.install_native_provider(&first.auth_header, vec![ProviderScript::new(
        "live fixture still owns its admitted Offering",
        move |request| request.path == "/v1/chat/completions" && request.body["model"] == fixture_model
            && request.body["stream"] == true,
        vec![ProviderResponse::OpenAi(json!({"choices":[{"index":0,"message":{"role":"assistant","content":final_reply},"finish_reason":"stop"}],"usage":{"prompt_tokens":42,"completion_tokens":7,"total_tokens":49}}))],
    )]).await;
    let (status, raw_sse) = stream_chat_full(
        &first.ctx.app,
        &first.auth_header,
        json!({
            "message": "prove the first fixture still owns its session and model",
            "session_id": &first_session_id,
            "model_selection": seeded_model_selection(&first.ctx),
            "execution_policy":{"turn_intent":"fixed_default","skill_auto_route":"disabled"}
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "first chat/stream: {raw_sse}");
    assert!(
        parse_sse_events(&raw_sse).iter().any(|event| {
            event["type"].as_str() == Some("text_delta")
                && event["content"].as_str() == Some(final_reply)
        }),
        "the first fixture did not complete its user turn: {raw_sse}"
    );

    second.ctx.close().await;
    stale.ctx.close().await;
    first.ctx.close().await;
}

pub async fn run_stream_corrects_invalid_model_policy_before_child_execution(explain: bool) {
    let b = bootstrap().await;
    let ctx = &b.ctx;
    let user_text =
        "Use the specified model for a child to calculate a value and report its result.";
    let child_task = "Calculate 13 times 17 and return only the integer.";
    let releases =
        ReleaseProviderGatesOnDrop(vec![std::sync::Arc::new(tokio::sync::Notify::new())]);
    let model = format!("mock-{}", ctx.suffix);
    let policy = json!({"mode":"fixed","selector":{"kind":"configured_name","model_name":model}});
    let corrected = json!({"action":"spawn","description":"Calculate a value","prompt":child_task,"requested_model_policy":policy});
    let mut invalid = corrected.clone();
    invalid["requested_model_policy"] = json!(policy.to_string());
    ctx.install_native_provider(
        &b.auth_header,
        vec![
            ProviderScript::new(
                "parent argument correction",
                move |request| {
                    request.path == "/v1/chat/completions"
                        && request.body["stream"] == true
                        && request.body["messages"].as_array().is_some_and(|messages| {
                            messages
                                .iter()
                                .any(|m| m["role"] == "user" && m["content"] == user_text)
                        })
                },
                vec![
                    native_tool_response(provider_tool_call("invalid-policy", "agent", invalid)),
                    native_tool_response(provider_tool_call(
                        "corrected-policy",
                        "agent",
                        corrected,
                    )),
                    native_tool_response(provider_tool_call(
                        "wait-for-result",
                        "agent",
                        json!({"action":"wait","timeout_ms":10000}),
                    )),
                    native_text_response("The observed child result is 221."),
                ],
            ),
            native_child_script(
                model.clone(),
                user_text,
                child_task,
                gated_native_delta(json!({"content":"221"}), "stop", releases.0[0].clone()),
            ),
        ],
    )
    .await;
    let payload = json!({
        "message":user_text,
        "explain":explain,
        "model_selection":seeded_model_selection(ctx),
        "execution_policy":{"turn_intent":"fixed_default","skill_auto_route":"disabled"}
    });
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(30);
    let response = tokio::time::timeout_at(
        deadline,
        ctx.app.clone().oneshot(
            Request::builder()
                .method("POST")
                .uri("/chat/stream")
                .header("authorization", &b.auth_header)
                .header("content-type", "application/json")
                .body(Body::from(payload.to_string()))
                .unwrap(),
        ),
    )
    .await
    .expect("bounded stream admission")
    .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let mut stream = response.into_body().into_data_stream();
    let mut bytes = Vec::new();
    let mut wait_observed = false;
    // This scenario requires an admitted wait. Hold the child response
    // until that receipt is published; a child arriving during inference
    // correctly supersedes the stale response at the Action fence.
    while let Some(chunk) = tokio::time::timeout_at(deadline, stream.next())
        .await
        .expect("bounded correction/wait/final sequence")
    {
        bytes.extend_from_slice(&chunk.unwrap());
        if !wait_observed
            && parse_sse_events(&String::from_utf8_lossy(&bytes))
                .iter()
                .any(|event| {
                    event["type"] == "tool_call_end"
                        && event["call_id"] == "wait-for-result"
                        && event["success"] == true
                })
        {
            wait_observed = true;
            releases.0[0].notify_one();
        }
    }
    assert!(
        wait_observed,
        "the wait must be admitted before releasing the child"
    );
    let raw = String::from_utf8(bytes).unwrap();
    let events = parse_sse_events(&raw);
    assert_eq!(
        events
            .iter()
            .filter(|e| e["type"] == "agent_spawned")
            .count(),
        1,
        "{raw}"
    );
    let requests = ctx.native_provider_requests().await;
    assert_eq!(
        requests
            .iter()
            .filter(|request| {
                request.body["messages"].as_array().is_some_and(|messages| {
                    messages
                        .iter()
                        .any(|m| m["role"] == "user" && m["content"] == user_text)
                })
            })
            .count(),
        4,
        "Explain capture must not change the correction/spawn/wait/final execution sequence"
    );
    let children: Vec<_> = requests
        .iter()
        .filter(|r| {
            r.body["messages"].as_array().is_some_and(|messages| {
                messages
                    .iter()
                    .any(|m| m["role"] == "user" && m["content"] == child_task)
            })
        })
        .collect();
    assert_eq!(
        children.len(),
        1,
        "only the corrected call may start a child"
    );
    assert_eq!(children[0].body["model"], model);
    let final_request = requests
            .iter()
            .find(|request| {
                request.body["messages"].as_array().is_some_and(|messages| {
                    messages
                        .iter()
                        .any(|message| message["tool_call_id"] == "wait-for-result")
                })
            })
            .unwrap_or_else(|| {
                let message_shapes: Vec<_> = requests
                    .iter()
                    .map(|request| {
                        request.body["messages"].as_array().map(|messages| {
                            messages
                                .iter()
                                .map(|message| {
                                    serde_json::json!({
                                        "role": message["role"],
                                        "tool_call_id": message["tool_call_id"],
                                        "tool_calls": message["tool_calls"],
                                    })
                                })
                                .collect::<Vec<_>>()
                        })
                    })
                    .collect();
                panic!(
                    "wait receipt must reach the final provider request; explain={explain}; messages={message_shapes:?}; events={raw}"
                );
            });
    let final_messages = final_request.body["messages"].as_array().unwrap();
    let wait: Value = final_messages
        .iter()
        .find(|message| message["tool_call_id"] == "wait-for-result")
        .and_then(|message| message["content"].as_str())
        .and_then(|content| serde_json::from_str(content).ok())
        .expect("typed wait receipt");
    assert_eq!(wait["success"], true);
    assert!(
        final_messages
            .iter()
            .filter_map(|message| message["content"].as_str())
            .flat_map(str::lines)
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .any(
                |payload| payload["context"]["schema"] == "direct_child_completion.v1"
                    && payload["context"]["children"]
                        .as_array()
                        .is_some_and(|children| {
                            children.iter().any(|child| {
                                child["status"] == "completed" && child["result"] == "221"
                            })
                        })
            ),
        "the final provider request must observe the actual completed child result"
    );
    let repair_request = requests
        .iter()
        .find(|r| {
            r.body["messages"].as_array().is_some_and(|messages| {
                messages
                    .iter()
                    .any(|m| m["tool_call_id"] == "invalid-policy")
            })
        })
        .expect("rejection must reach the next actual provider request");
    let receipt: Value = repair_request.body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["tool_call_id"] == "invalid-policy")
        .and_then(|m| m["content"].as_str())
        .and_then(|s| serde_json::from_str(s).ok())
        .expect("typed model-facing rejection");
    assert_eq!(receipt["executed"], false);
    assert_eq!(receipt["retryable"], false);
    assert_eq!(receipt["error_kind"], "tool_invalid_args");
    assert_eq!(
        receipt["recovery_evidence"]["recovery_actions"],
        json!(["correct_arguments"])
    );
    let agent_schema = repair_request.body["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["function"]["name"] == "agent")
        .expect("actual provider schema");
    assert_eq!(
        agent_schema["function"]["parameters"]["properties"]["requested_model_policy"]["type"],
        "object"
    );
    assert!(
        events.iter().any(|e| e["type"] == "text_delta"
            && e["content"].as_str().is_some_and(|s| s.contains("221"))),
        "{raw}"
    );
    let session_id = sse_first_data_json_with_type(&raw, "session_info").unwrap()["session_id"]
        .as_str()
        .unwrap()
        .to_string();
    cleanup_session_data(&ctx.shared_pool, &ctx.user_id, &session_id).await;
    ctx.close().await;
}

/// B1: Session row persists, chat/stream completes, run status is queryable.
pub async fn run_stream_session_and_run_status() {
    let b = bootstrap().await;
    let ctx = &b.ctx;
    let app = &ctx.app;
    let auth = &b.auth_header;
    let pool = &ctx.pool;
    let user_id = &ctx.user_id;

    // Create a fresh session for isolation.
    let (st_sess, sess) = post_json(
        app,
        "/sessions",
        Some(auth.as_str()),
        json!({ "title": "phase-b session+run", "metadata": { "suite": "stream_persistence" } }),
    )
    .await;
    assert_eq!(st_sess, StatusCode::CREATED, "create session: {sess}");
    let session_id = sess["session_id"].as_str().expect("session_id").to_string();

    // ── Verify session row exists in DB ──
    let row = sqlx::query(
        "SELECT user_id, status FROM agent_sessions WHERE session_id = ? AND user_id = ?",
    )
    .bind(&session_id)
    .bind(user_id)
    .fetch_optional(pool)
    .await
    .expect("select agent_sessions");
    let row = row.expect("agent_sessions row should exist after POST /sessions");
    assert_eq!(
        row.try_get::<String, _>("user_id").ok().as_deref(),
        Some(user_id.as_str()),
        "session user_id should match"
    );

    // Complete a real provider-backed Server turn before checking its durable state.
    let fixture_model = format!("mock-{}", ctx.suffix);
    ctx.install_native_provider(auth,vec![ProviderScript::new("run_stream_session_and_run_status",move |request| request.path=="/v1/chat/completions" && request.body["model"]==fixture_model && request.body["stream"]==true && request.body["messages"].as_array().is_some_and(|messages| messages.iter().any(|message|message["role"]=="user" && message["content"]=="phase-b persistence probe")),vec![ProviderResponse::OpenAi(json!({"choices":[{"index":0,"message":{"role":"assistant","content":"Persistence verified.","reasoning_content":"","tool_calls":[]},"finish_reason":"stop"}],"usage":{"prompt_tokens":42,"completion_tokens":7,"total_tokens":49}}))])]).await;
    let payload = json!({
        "message": "phase-b persistence probe",
    "execution_policy":{"turn_intent":"fixed_default","skill_auto_route":"disabled"},
        "session_id": &session_id,
        "model_selection": seeded_model_selection(ctx),
        "context": {

        }
    });
    let (status, body) = stream_chat_full(app, auth, payload).await;
    assert_eq!(status, StatusCode::OK, "chat/stream should return 200");
    assert!(body.contains("data: "), "should have SSE data frames");

    // Extract run_id from session_info event.
    let si = sse_first_data_json_with_type(&body, "session_info");
    assert!(si.is_some(), "should have session_info SSE event");
    let si = si.unwrap();
    let run_id = si["run_id"].as_str().expect("run_id in session_info");
    let sse_session_id = si["session_id"]
        .as_str()
        .expect("session_id in session_info");
    assert_eq!(sse_session_id, session_id, "session_id should match");

    // ── Verify run status after stream completes ──
    // The background task may not have finalized yet — poll briefly.
    let mut run_status = String::new();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    while tokio::time::Instant::now() < deadline {
        let (st_run, run_json) = get_json(
            app,
            &format!("/chat/runs/{run_id}"),
            Some(auth.as_str()),
            &[],
        )
        .await;
        assert_eq!(st_run, StatusCode::OK, "get run status: {run_json}");
        assert_eq!(run_json["run_id"].as_str(), Some(run_id));
        assert_eq!(run_json["session_id"].as_str(), Some(session_id.as_str()));
        run_status = run_json["status"].as_str().unwrap_or("").to_string();
        if run_status == "completed" {
            let events_count = run_json["events_count"].as_i64().unwrap_or(0);
            assert!(
                events_count > 0,
                "completed run should have events_count > 0, got {events_count}"
            );
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
    assert_eq!(
        run_status, "completed",
        "run should be completed after stream finishes"
    );

    // ── Cleanup ──
    cleanup_session_data(&ctx.shared_pool, user_id, &session_id).await;
    ctx.close().await;
}

/// Online product gate for structured fan-in. This intentionally crosses the
/// full `/chat/stream` + dynamic child runtime + MatrixOne durability path:
/// three child runs must exist and settle before the one parent synthesis,
/// without fabricating a detached reconciliation turn or orphan transcript
/// ownership.
pub async fn run_stream_structured_fanout_has_one_parent_synthesis_and_durable_tree() {
    let b = bootstrap().await;
    let ctx = &b.ctx;
    let app = &ctx.app;
    let auth = &b.auth_header;
    let pool = &ctx.pool;
    let user_id = &ctx.user_id;

    let (status, session) = post_json(
        app,
        "/sessions",
        Some(auth.as_str()),
        json!({
            "title": "structured fan-in online gate",
            "metadata": {"suite": "structured_fanout_online"}
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "create session: {session}");
    let session_id = session["session_id"]
        .as_str()
        .expect("session_id")
        .to_string();

    let final_reply = "One parent synthesis after all three durable child results.";
    let fanout_args = json!({
        "action": "start",
        "group_id": "online-review-group",
        "title": "Online three-way review",
        "target_count": 3,
        "slots": [
            {
                "id": "storage",
                "description": "Online storage review",
                "prompt": "Inspect storage behavior and return one finding.",
                "agent_type": "general-purpose"
            },
            {
                "id": "runtime",
                "description": "Online runtime review",
                "prompt": "Inspect runtime behavior and return one finding.",
                "agent_type": "general-purpose"
            },
            {
                "id": "journey",
                "description": "Online journey review",
                "prompt": "Inspect the user journey and return one finding.",
                "agent_type": "general-purpose"
            }
        ]
    });
    let fixture_model = format!("mock-{}", ctx.suffix);
    let root_model = fixture_model.clone();
    let child_model = fixture_model.clone();
    let releases = ReleaseProviderGatesOnDrop(
        (0..4)
            .map(|_| std::sync::Arc::new(tokio::sync::Notify::new()))
            .collect(),
    );
    let child_markers = [
        "durable storage finding",
        "durable runtime finding",
        "durable journey finding",
    ];
    ctx.install_native_provider(
        auth,
        vec![
            ProviderScript::new(
                "actual parent fanout execution",
                move |request| {
                    request.path == "/v1/chat/completions"
                        && request.body["model"] == root_model
                        && request.body["stream"] == true
                        && delegation_assessment(&request.body).is_none()
                        && request.body["messages"].as_array().is_some_and(|messages| {
                            messages.iter().any(|message| {
                                message["role"] == "user"
                                    && message["content"]
                                        == "Run three reviews as one structured work group."
                            })
                        })
                },
                vec![
                    native_search_response("online-fanout-search", "agent_fanout"),
                    native_invoke_response("online-fanout-start", "agent_fanout", fanout_args),
                    gated_native_delta(json!({"tool_calls":[{"index":0,"id":"online-fanout-wait","type":"function","function":{"name":"invoke_tool","arguments":json!({"name":"agent","arguments":{"action":"wait","timeout_ms":10000}}).to_string()}}]}), "tool_calls", releases.0[3].clone()),
                    native_text_response(final_reply),
                ],
            ),
            native_child_script(
                child_model.clone(),
                "Run three reviews as one structured work group.",
                "Inspect storage behavior and return one finding.",
                gated_native_delta(json!({"content":child_markers[0]}), "stop", releases.0[0].clone()),
            ),
            native_child_script(
                child_model.clone(),
                "Run three reviews as one structured work group.",
                "Inspect runtime behavior and return one finding.",
                gated_native_delta(json!({"content":child_markers[1]}), "stop", releases.0[1].clone()),
            ),
            native_child_script(
                child_model.clone(),
                "Run three reviews as one structured work group.",
                "Inspect the user journey and return one finding.",
                gated_native_delta(json!({"content":child_markers[2]}), "stop", releases.0[2].clone()),
            ),
        ],
    )
    .await;
    let payload = json!({
        "message": "Run three reviews as one structured work group.",
    "execution_policy":{"turn_intent":"fixed_default","skill_auto_route":"disabled"},
        "session_id": &session_id,
        "model_selection": seeded_model_selection(ctx),
        "context": {

        }
    });
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(15);
    let response = tokio::time::timeout_at(
        deadline,
        app.clone().oneshot(
            Request::builder()
                .method("POST")
                .uri("/chat/stream")
                .header("authorization", auth.as_str())
                .header("content-type", "application/json")
                .body(Body::from(payload.to_string()))
                .unwrap(),
        ),
    )
    .await
    .expect("bounded stream admission")
    .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let mut stream = response.into_body().into_data_stream();
    let mut bytes = Vec::new();
    // The third root response stays in flight until every real child result
    // arrives. This exercises exactly one Action-fence supersession, rather
    // than depending on how a scheduler interleaves three independent inputs.
    loop {
        let requests = ctx.native_provider_requests().await;
        let entered = requests
            .iter()
            .filter(|request| {
                request.body["stream"] == true && delegation_assessment(&request.body).is_none()
            })
            .count();
        if entered == 6 {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "all real provider requests must enter"
        );
        tokio::select! {
            chunk = stream.next() => {
                bytes.extend_from_slice(&chunk.expect("stream before provider entry").unwrap());
            }
            () = tokio::time::sleep(std::time::Duration::from_millis(10)) => {}
        }
    }
    for gate in &releases.0[..3] {
        gate.notify_one();
    }
    loop {
        let chunk = tokio::time::timeout_at(deadline, stream.next())
            .await
            .unwrap()
            .expect("live stream before child terminals")
            .unwrap();
        bytes.extend_from_slice(&chunk);
        let events = parse_sse_events(&String::from_utf8_lossy(&bytes));
        if events
            .iter()
            .filter(|event| {
                event["type"] == "agent_completed" || event["event_type"] == "agent_completed"
            })
            .count()
            == 3
        {
            break;
        }
    }
    releases.0[3].notify_one();
    while let Some(chunk) = tokio::time::timeout_at(deadline, stream.next())
        .await
        .unwrap()
    {
        bytes.extend_from_slice(&chunk.unwrap());
    }
    let raw_sse = String::from_utf8(bytes).unwrap();
    let requests = ctx.native_provider_requests().await;
    let root: Vec<_> = requests
        .iter()
        .filter(|request| {
            request.body["stream"] == true
                && delegation_assessment(&request.body).is_none()
                && request.body["messages"].as_array().is_some_and(|messages| {
                    messages.iter().any(|m| {
                        m["role"] == "user"
                            && m["content"] == "Run three reviews as one structured work group."
                    })
                })
        })
        .collect();
    assert_eq!(root.len(), 4, "one supersession and one parent synthesis");
    let final_context = root[3].body["messages"].to_string();
    assert!(
        final_context.contains("runtime_input_action_fence.v1"),
        "the in-flight response must be fenced by real child input"
    );
    for marker in child_markers {
        assert!(
            final_context.contains(marker),
            "final request must include {marker}"
        );
    }
    assert_native_delegation_without_auxiliary_selector(
        ctx,
        "Run three reviews as one structured work group.",
        &[
            (
                "Online storage review",
                "Inspect storage behavior and return one finding.",
            ),
            (
                "Online runtime review",
                "Inspect runtime behavior and return one finding.",
            ),
            (
                "Online journey review",
                "Inspect the user journey and return one finding.",
            ),
        ],
    )
    .await;
    let events = parse_sse_events(&raw_sse);
    let session_info = events
        .iter()
        .find(|event| event["type"].as_str() == Some("session_info"))
        .unwrap_or_else(|| panic!("missing session_info: {raw_sse}"));
    let root_run_id = session_info["run_id"]
        .as_str()
        .expect("root run_id")
        .to_string();
    let child_terminal_positions = events
        .iter()
        .enumerate()
        .filter(|(_, event)| {
            event["type"].as_str() == Some("agent_completed")
                || event["event_type"].as_str() == Some("agent_completed")
        })
        .map(|(index, _)| index)
        .collect::<Vec<_>>();
    assert_eq!(
        child_terminal_positions.len(),
        3,
        "each child must have one terminal projection: {raw_sse}"
    );
    let final_positions = events
        .iter()
        .enumerate()
        .filter(|(_, event)| {
            event["type"].as_str() == Some("text_delta")
                && event["content"].as_str() == Some(final_reply)
        })
        .map(|(index, _)| index)
        .collect::<Vec<_>>();
    assert_eq!(
        final_positions.len(),
        1,
        "the root must synthesize exactly once: {raw_sse}"
    );
    assert!(
        child_terminal_positions
            .iter()
            .all(|position| *position < final_positions[0]),
        "parent synthesis appeared before the complete fanout: {raw_sse}"
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| event["type"].as_str() == Some("turn_complete"))
            .count(),
        1,
        "one user turn must have one terminal boundary"
    );

    let durable_deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(20);
    loop {
        let completed: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM agent_runs \
             WHERE user_id = ? AND session_id = ? AND status = 'completed'",
        )
        .bind(user_id)
        .bind(&session_id)
        .fetch_one(pool)
        .await
        .unwrap_or(0);
        if completed == 4 {
            break;
        }
        assert!(
            tokio::time::Instant::now() < durable_deadline,
            "expected one root plus three completed durable child runs, got {completed}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }

    let runs = sqlx::query(
        "SELECT run_id, parent_run_id, root_run_id, depth, status \
         FROM agent_runs WHERE user_id = ? AND session_id = ? \
         ORDER BY depth ASC, run_id ASC",
    )
    .bind(user_id)
    .bind(&session_id)
    .fetch_all(pool)
    .await
    .expect("load durable run tree");
    assert_eq!(runs.len(), 4, "durable run tree: {runs:?}");
    let root = runs
        .iter()
        .find(|run| run.get::<i32, _>("depth") == 0)
        .expect("root run row");
    assert_eq!(root.get::<String, _>("run_id"), root_run_id);
    assert_eq!(root.get::<String, _>("root_run_id"), root_run_id);
    assert!(root.get::<Option<String>, _>("parent_run_id").is_none());
    let children = runs
        .iter()
        .filter(|run| run.get::<i32, _>("depth") == 1)
        .collect::<Vec<_>>();
    assert_eq!(children.len(), 3, "durable child runs: {runs:?}");
    assert!(children.iter().all(|run| {
        run.get::<Option<String>, _>("parent_run_id").as_deref() == Some(root_run_id.as_str())
            && run.get::<String, _>("root_run_id") == root_run_id
            && run.get::<String, _>("status") == "completed"
    }));

    let runtime_reconciliations: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM agent_events \
         WHERE user_id = ? AND session_id = ? AND event_type = 'runtime_reconciliation'",
    )
    .bind(user_id)
    .bind(&session_id)
    .fetch_one(pool)
    .await
    .expect("count runtime reconciliation events");
    assert_eq!(
        runtime_reconciliations, 0,
        "foreground fan-in must remain on the original root turn"
    );

    let assistant_rows = sqlx::query(
        "SELECT content, payload_json FROM session_transcript_items \
         WHERE user_id = ? AND session_id = ? AND role = 'assistant' \
         ORDER BY item_seq ASC",
    )
    .bind(user_id)
    .bind(&session_id)
    .fetch_all(pool)
    .await
    .expect("load assistant transcript rows");
    let semantically_blank = assistant_rows
        .iter()
        .filter(|row| row.get::<String, _>("content").trim().is_empty())
        .filter(|row| {
            row.get::<Option<String>, _>("payload_json")
                .and_then(|payload| serde_json::from_str::<Value>(&payload).ok())
                .and_then(|payload| payload["tool_calls"].as_array().cloned())
                .is_none_or(|tool_calls| tool_calls.is_empty())
        })
        .count();
    assert_eq!(
        semantically_blank, 0,
        "an assistant transcript row must contain text or structured tool calls"
    );
    assert!(
        assistant_rows.iter().any(|row| {
            row.get::<String, _>("content").trim().is_empty()
                && row
                    .get::<Option<String>, _>("payload_json")
                    .and_then(|payload| serde_json::from_str::<Value>(&payload).ok())
                    .and_then(|payload| payload["tool_calls"].as_array().cloned())
                    .is_some_and(|tool_calls| {
                        tool_calls.len() == 1
                            && tool_calls[0]["name"].as_str() == Some("agent_fanout")
                    })
        }),
        "the tool-only root boundary must retain its typed fanout call instead of a semantically empty row"
    );
    let (transcript_status, transcript) = get_json(
        app,
        &format!("/sessions/{session_id}/transcript?scope=root_conversation&limit=20"),
        Some(auth.as_str()),
        &[],
    )
    .await;
    assert_eq!(
        transcript_status,
        StatusCode::OK,
        "root transcript: {transcript}"
    );
    assert!(
        transcript["items"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|item| {
                item["role"].as_str() == Some("assistant")
                    && item["content"].as_str() == Some("")
                    && item["tool_calls"].as_array().is_some_and(|calls| {
                        calls.len() == 1 && calls[0]["name"].as_str() == Some("agent_fanout")
                    })
            }),
        "the downstream transcript API must expose the typed fanout call: {transcript}"
    );
    let orphan_transcript_runs: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM session_transcript_items AS transcript \
         LEFT JOIN agent_runs AS run \
           ON run.user_id = transcript.user_id \
          AND run.session_id = transcript.session_id \
          AND run.run_id = transcript.run_id \
         WHERE transcript.user_id = ? AND transcript.session_id = ? \
           AND transcript.run_id IS NOT NULL AND run.run_id IS NULL",
    )
    .bind(user_id)
    .bind(&session_id)
    .fetch_one(pool)
    .await
    .expect("count orphan transcript run identities");
    assert_eq!(
        orphan_transcript_runs, 0,
        "every non-null transcript run_id must resolve to the same durable run tree"
    );

    cleanup_session_data(&ctx.shared_pool, user_id, &session_id).await;
    ctx.close().await;
}

/// Four live fanouts intentionally reuse the same group/slot labels across
/// two users and two sessions per user. The registry, stream projection, and
/// durable tree must remain scoped by ownership rather than by presentation
/// identifiers while successful and rejected-provider groups settle together.
pub async fn run_stream_concurrent_fanout_isolates_users_sessions_and_group_ids() {
    let b = bootstrap().await;
    let ctx = &b.ctx;
    let app = &ctx.app;
    let pool = &ctx.pool;

    let (status,a_first_session)=post_json(app,"/sessions",Some(b.auth_header.as_str()),json!({"title":"fanout isolation A session 1","metadata":{"suite":"fanout_concurrent_isolation","full_llm_capture":true}})).await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "create A first capture session: {a_first_session}"
    );
    let a_first_session_id = a_first_session["session_id"].as_str().unwrap().to_string();
    let (status, a_second_session) = post_json(
        app,
        "/sessions",
        Some(b.auth_header.as_str()),
        json!({
            "title": "fanout isolation A second session",
            "metadata": {"suite": "fanout_concurrent_isolation", "full_llm_capture": true}
        }),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "create A second session: {a_second_session}"
    );
    let a_second_session_id = a_second_session["session_id"]
        .as_str()
        .expect("A second session_id")
        .to_string();

    let suffix = Uuid::new_v4().simple().to_string();
    let b_username = format!("prod_matrix_fanout_iso_{suffix}");
    let (status, registration) = post_json(
        app,
        "/auth/register",
        None,
        json!({
            "username": b_username,
            "email": format!("{b_username}@e2e.test"),
            "password": E2E_PASSWORD,
            "display_name": "Fanout isolation user B"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "register B: {registration}");
    let b_user_id = registration["user_id"]
        .as_str()
        .expect("B user_id")
        .to_string();
    let b_auth = format!(
        "Bearer {}",
        registration["access_token"]
            .as_str()
            .expect("B access token")
    );
    let mut b_session_ids = Vec::with_capacity(2);
    for ordinal in 1..=2 {
        let (status, session) = post_json(
            app,
            "/sessions",
            Some(b_auth.as_str()),
            json!({
                "title": format!("fanout isolation B session {ordinal}"),
                "metadata": {"suite": "fanout_concurrent_isolation", "full_llm_capture": true}
            }),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::CREATED,
            "create B session {ordinal}: {session}"
        );
        b_session_ids.push(
            session["session_id"]
                .as_str()
                .expect("B session_id")
                .to_string(),
        );
    }

    let cases = [
        ConcurrentFanoutCase {
            name: "a-one-success".to_string(),
            auth: b.auth_header.clone(),
            user_id: ctx.user_id.clone(),
            session_id: a_first_session_id.clone(),
            terminal: OnlineFanoutTerminal::Completed,
            final_reply: "A1 synthesized its two successful children once.".to_string(),
            child_provenance: "A1 child evidence belongs only to A1.".to_string(),
        },
        ConcurrentFanoutCase {
            name: "a-two-failure".to_string(),
            auth: b.auth_header.clone(),
            user_id: ctx.user_id.clone(),
            session_id: a_second_session_id.clone(),
            terminal: OnlineFanoutTerminal::Failed,
            final_reply: "A2 synthesized its two preserved failures once.".to_string(),
            child_provenance: "A2 private provider body marker.".to_string(),
        },
        ConcurrentFanoutCase {
            name: "b-one-failure".to_string(),
            auth: b_auth.clone(),
            user_id: b_user_id.clone(),
            session_id: b_session_ids[0].clone(),
            terminal: OnlineFanoutTerminal::Failed,
            final_reply: "B1 synthesized its two preserved failures once.".to_string(),
            child_provenance: "B1 private provider body marker.".to_string(),
        },
        ConcurrentFanoutCase {
            name: "b-two-success".to_string(),
            auth: b_auth.clone(),
            user_id: b_user_id.clone(),
            session_id: b_session_ids[1].clone(),
            terminal: OnlineFanoutTerminal::Completed,
            final_reply: "B2 synthesized its two successful children once.".to_string(),
            child_provenance: "B2 child evidence belongs only to B2.".to_string(),
        },
    ];
    let shared_group_id = "same-group-label-across-four-live-roots";
    let scripts = cases
        .iter()
        .flat_map(|case| concurrent_fanout_scripts(ctx, case, shared_group_id))
        .collect::<Vec<_>>();
    ctx.install_native_provider(&b.auth_header, scripts).await;
    let payloads = cases
        .iter()
        .map(|case| concurrent_fanout_payload(ctx, case))
        .collect::<Vec<_>>();
    let started = tokio::time::Instant::now();
    let responses =
        futures_util::future::join_all(cases.iter().zip(payloads).map(|(case, payload)| {
            stream_chat_full_with_timeout(app, case.auth.as_str(), payload, 20)
        }))
        .await;
    assert!(
        started.elapsed() < std::time::Duration::from_secs(20),
        "four bounded fanouts exceeded the online concurrency budget"
    );

    for case in &cases {
        assert_native_delegation_without_auxiliary_selector(
            ctx,
            &format!("Run the isolated fanout scenario {}.", case.name),
            &[
                (
                    "First isolated child",
                    &format!("{}: Return the first independent finding.", case.name),
                ),
                (
                    "Second isolated child",
                    &format!("{}: Return the second independent finding.", case.name),
                ),
            ],
        )
        .await;
    }

    let all_child_provenances = cases
        .iter()
        .map(|case| case.child_provenance.clone())
        .collect::<Vec<_>>();
    let mut evidence = Vec::with_capacity(cases.len());
    for (case, (status, raw_sse)) in cases.iter().zip(responses) {
        assert_eq!(status, StatusCode::OK, "{}: {raw_sse}", case.name);
        evidence.push(
            assert_concurrent_fanout_stream(
                pool,
                case,
                shared_group_id,
                &all_child_provenances,
                &raw_sse,
            )
            .await,
        );
    }
    let all_run_ids = evidence
        .iter()
        .flat_map(|item| {
            std::iter::once(item.root_run_id.as_str())
                .chain(item.child_run_ids.iter().map(String::as_str))
        })
        .collect::<std::collections::HashSet<_>>();
    assert_eq!(
        all_run_ids.len(),
        12,
        "four isolated roots plus eight slots require twelve unique run identities"
    );

    for (case, item) in cases.iter().zip(&evidence) {
        assert_concurrent_fanout_durability(pool, case, item).await;
        let foreign_auth = if case.user_id == ctx.user_id {
            b_auth.as_str()
        } else {
            b.auth_header.as_str()
        };
        let (status, body) = get_json(
            app,
            &format!("/chat/runs/{}", item.root_run_id),
            Some(foreign_auth),
            &[],
        )
        .await;
        assert_eq!(
            status,
            StatusCode::NOT_FOUND,
            "{}: a foreign user observed its root run: {body}",
            case.name
        );
    }

    cleanup_session_data(&ctx.shared_pool, &ctx.user_id, &a_first_session_id).await;
    cleanup_session_data(&ctx.shared_pool, &ctx.user_id, &a_second_session_id).await;
    for session_id in &b_session_ids {
        cleanup_session_data(&ctx.shared_pool, &b_user_id, session_id).await;
    }
    ctx.close().await;
}

/// Cancelling a live root while every fanout child is inside provider latency
/// must settle the already-accepted fixed group once. No delayed child may
/// publish success and the cancelled root must never advance to synthesis.
pub async fn run_stream_root_cancel_settles_slow_fanout_without_late_synthesis() {
    let b = bootstrap().await;
    let ctx = &b.ctx;
    let app = &ctx.app;
    let auth = b.auth_header.clone();
    let user_id = ctx.user_id.clone();

    let (status, session) = post_json(
        app,
        "/sessions",
        Some(auth.as_str()),
        json!({
            "title": "slow fanout root cancel gate",
            "metadata": {"suite": "fanout_root_cancel_online"}
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "create session: {session}");
    let session_id = session["session_id"]
        .as_str()
        .expect("session_id")
        .to_string();
    let forbidden_late_reply = "This synthesis must not appear after root cancellation.";
    let fixture_model = format!("mock-{}", ctx.suffix);
    let root_model = fixture_model.clone();
    let child_model = fixture_model.clone();
    let releases = ReleaseProviderGatesOnDrop(
        (0..3)
            .map(|_| std::sync::Arc::new(tokio::sync::Notify::new()))
            .collect::<Vec<_>>(),
    );
    let mut child_responses:Vec<_>=releases.0.iter().map(|release|ProviderResponse::Stream { content_type:"text/event-stream", chunks:vec![format!("data: {}\n\n",json!({"choices":[{"index":0,"delta":{"content":"late child output must be suppressed"}}]})).into_bytes(),format!("data: {}\n\n",json!({"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]})).into_bytes(),b"data: [DONE]\n\n".to_vec()],release_before_chunk:Some((0,release.clone())) }).collect();
    ctx.install_native_provider(&auth,vec![
        ProviderScript::new("cancel root exact requests",move |request|request.path=="/v1/chat/completions" && request.body["model"]==root_model && request.body["stream"]==true && delegation_assessment(&request.body).is_none() && request.body["messages"].as_array().is_some_and(|messages|messages.iter().any(|m|m["role"]=="user" && m["content"]=="Start three slow reviews; the user will cancel the live root.")),vec![native_search_response("slow-fanout-tool-search", "agent_fanout"),native_invoke_response(
                    "slow-fanout-before-root-cancel",
                    "agent_fanout",
                    json!({
                        "action": "start",
                        "group_id": "slow-fanout-root-cancel",
                        "title": "Slow fanout cancellation gate",
                        "target_count": 3,
                        "slots": [
                            {"id": "one", "description": "Slow child one", "prompt": "Return finding one."},
                            {"id": "two", "description": "Slow child two", "prompt": "Return finding two."},
                            {"id": "three", "description": "Slow child three", "prompt": "Return finding three."}
                        ],
                        "defaults": {"agent_type": "general-purpose"}
                    }),
                ),native_invoke_response("slow-fanout-wait","agent",json!({"action":"wait","timeout_ms":10000}))]),
        native_child_script(child_model.clone(),"Start three slow reviews; the user will cancel the live root.","Return finding one.",child_responses.remove(0)),
native_child_script(child_model.clone(),"Start three slow reviews; the user will cancel the live root.","Return finding two.",child_responses.remove(0)),
native_child_script(child_model.clone(),"Start three slow reviews; the user will cancel the live root.","Return finding three.",child_responses.remove(0)),
    ]).await;
    let payload = json!({
        "message": "Start three slow reviews; the user will cancel the live root.","execution_policy":{"turn_intent":"fixed_default","skill_auto_route":"disabled"},
        "session_id": session_id,
        "model_selection": seeded_model_selection(ctx),
        "context": {

        }
    });
    let started = tokio::time::Instant::now();
    let test_secret = std::env::var("ASTRA_TEST_E2E_SECRET").expect("bridge test secret");
    let request = Request::builder()
        .method("POST")
        .uri("/chat/stream")
        .header("authorization", auth.as_str())
        .header("content-type", "application/json")
        .header("x-astra-e2e-test-secret", test_secret)
        .body(Body::from(payload.to_string()))
        .expect("root cancellation stream request");
    let response = app
        .clone()
        .oneshot(request)
        .await
        .expect("root cancellation stream response");
    let stream_status = response.status();
    assert_eq!(stream_status, StatusCode::OK);
    let mut stream = response.into_body().into_data_stream();
    let mut stream_bytes = Vec::new();

    // `agent_spawned` is emitted before activation and therefore cannot prove
    // that provider cancellation was exercised. Wait for the canonical live
    // progress signal emitted immediately before each child LLM call instead.
    let provider_entry_deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    let provider_child_ids = loop {
        let chunk = tokio::time::timeout_at(provider_entry_deadline, stream.next())
            .await
            .expect("all fanout children must enter provider I/O before cancellation")
            .expect("stream ended before all fanout children entered provider I/O")
            .expect("root cancellation stream chunk");
        stream_bytes.extend_from_slice(&chunk);
        let raw = String::from_utf8_lossy(&stream_bytes);
        let child_ids = parse_sse_events(raw.as_ref())
            .into_iter()
            .filter(|event| {
                event["type"].as_str() == Some("agent_progress")
                    && event["status"].as_str() == Some("llm_call_started")
            })
            .filter_map(|event| event["agent_id"].as_str().map(ToString::to_string))
            .collect::<std::collections::HashSet<_>>();
        if child_ids.len() == 3 {
            break child_ids;
        }
    };

    let root_run_id: String = sqlx::query_scalar(
        "SELECT run_id FROM agent_runs \
         WHERE user_id = ? AND session_id = ? AND depth = 0 \
         ORDER BY created_at DESC LIMIT 1",
    )
    .bind(&user_id)
    .bind(&session_id)
    .fetch_one(&ctx.pool)
    .await
    .expect("load live root run after provider entry");
    let child_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM agent_runs \
         WHERE user_id = ? AND session_id = ? AND depth = 1",
    )
    .bind(&user_id)
    .bind(&session_id)
    .fetch_one(&ctx.pool)
    .await
    .expect("count provider-entered slow fanout children");
    assert_eq!(child_count, 3);
    assert_eq!(provider_child_ids.len(), 3);

    loop {
        let requests = ctx.native_provider_requests().await;
        let calls = requests
            .iter()
            .filter(|request| {
                request.body["stream"] == true && delegation_assessment(&request.body).is_none()
            })
            .collect::<Vec<_>>();
        let root = calls
            .iter()
            .filter(|request| {
                request.body["messages"].as_array().is_some_and(|messages| {
                    messages.iter().any(|m| {
                        m["role"] == "user"
                            && m["content"]
                                == "Start three slow reviews; the user will cancel the live root."
                    })
                })
            })
            .count();
        if root == 3 && calls.len() == 6 {
            break;
        }
        assert!(
            tokio::time::Instant::now() < provider_entry_deadline,
            "three child HTTP requests and parent wait must enter before cancellation"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_native_delegation_without_auxiliary_selector(
        ctx,
        "Start three slow reviews; the user will cancel the live root.",
        &[
            ("Slow child one", "Return finding one."),
            ("Slow child two", "Return finding two."),
            ("Slow child three", "Return finding three."),
        ],
    )
    .await;

    let (cancel_status, cancel_body) = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        delete_json(
            app,
            &format!("/chat/runs/{root_run_id}"),
            Some(auth.as_str()),
        ),
    )
    .await
    .expect("root cancellation exceeded its bounded HTTP deadline");
    assert_eq!(
        cancel_status,
        StatusCode::OK,
        "cancel live fanout root: {cancel_body}"
    );
    assert_eq!(
        cancel_body["status"].as_str(),
        Some("cancellation_requested"),
        "a live owner acknowledges intent before executor convergence: {cancel_body}"
    );
    assert_eq!(
        cancel_body["execution_settled"].as_bool(),
        Some(false),
        "the HTTP receipt must not claim a terminal state before the live executor settles"
    );

    let stream_deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        match tokio::time::timeout_at(stream_deadline, stream.next()).await {
            Ok(Some(chunk)) => {
                stream_bytes.extend_from_slice(&chunk.expect("cancelled fanout stream chunk"));
            }
            Ok(None) => break,
            Err(_) => panic!("cancelled fanout stream did not terminate"),
        }
    }
    drop(releases);
    let raw_sse = String::from_utf8_lossy(&stream_bytes).into_owned();
    assert_eq!(stream_status, StatusCode::OK, "cancelled stream: {raw_sse}");
    assert!(
        started.elapsed() < std::time::Duration::from_secs(15),
        "root cancellation did not bound the slow fanout"
    );
    let events = parse_sse_events(&raw_sse);
    assert_eq!(
        events
            .iter()
            .filter(|event| event["type"].as_str() == Some("agent_spawned"))
            .count(),
        3,
        "all accepted slots must be projected exactly once: {raw_sse}"
    );
    let cancelled = events
        .iter()
        .filter(|event| event["type"].as_str() == Some("agent_cancelled"))
        .collect::<Vec<_>>();
    // The root waits for cancellation ownership transfer, while each child
    // commits its terminal asynchronously. Verify any live terminals here;
    // the exact all-child terminal contract belongs to durable replay below.
    assert!(
        cancelled.len() <= 3,
        "duplicate cancellation terminals: {raw_sse}"
    );
    assert!(
        cancelled.iter().all(|event| {
            event["reason"]
                .as_str()
                .is_some_and(|reason| !reason.trim().is_empty() && reason.contains("cancel"))
        }),
        "child cancellation provenance must remain explicit: {raw_sse}"
    );
    assert!(
        cancelled
            .iter()
            .all(|event| event["cancellation_origin"].as_str() == Some("user")),
        "root DELETE must project the canonical user origin for every provider-entered child: {raw_sse}"
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| {
                event["type"].as_str() == Some("text_delta")
                    && event["content"].as_str() == Some(forbidden_late_reply)
            })
            .count(),
        0,
        "a cancelled root must not advance to parent synthesis: {raw_sse}"
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| {
                event["type"].as_str() == Some("text_delta")
                    && event["content"].as_str() == Some("late child output must be suppressed")
            })
            .count(),
        0,
        "cancelled provider latency must not leak late child output: {raw_sse}"
    );

    let settlement_deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    let rows = loop {
        let rows = sqlx::query(
            "SELECT run_id, parent_run_id, root_run_id, depth, status \
             FROM agent_runs WHERE user_id = ? AND session_id = ? \
             ORDER BY depth ASC, run_id ASC",
        )
        .bind(&user_id)
        .bind(&session_id)
        .fetch_all(&ctx.pool)
        .await
        .expect("load cancelled fanout tree");
        if rows.len() == 4
            && rows
                .iter()
                .all(|row| row.get::<String, _>("status") == "cancelled")
        {
            break rows;
        }
        assert!(
            tokio::time::Instant::now() < settlement_deadline,
            "cancelled fanout tree did not settle exactly once: {rows:?}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    };
    assert_eq!(rows.len(), 4, "cancelled durable fanout tree: {rows:?}");
    assert_eq!(
        rows.iter()
            .filter(|row| row.get::<i32, _>("depth") == 0)
            .count(),
        1
    );
    assert!(
        rows.iter()
            .filter(|row| row.get::<i32, _>("depth") == 1)
            .all(|row| {
                row.get::<Option<String>, _>("parent_run_id").as_deref()
                    == Some(root_run_id.as_str())
                    && row.get::<String, _>("root_run_id") == root_run_id
            })
    );
    let durable_child_ids = rows
        .iter()
        .filter(|row| row.get::<i32, _>("depth") == 1)
        .map(|row| row.get::<String, _>("run_id"))
        .collect::<std::collections::HashSet<_>>();
    assert_eq!(durable_child_ids.len(), 3, "one durable run per fixed slot");
    let streamed_child_ids = cancelled
        .iter()
        .filter_map(|event| event["run_id"].as_str().map(ToString::to_string))
        .collect::<std::collections::HashSet<_>>();
    assert_eq!(
        streamed_child_ids.len(),
        cancelled.len(),
        "live child cancellation identities must not repeat"
    );
    assert!(
        streamed_child_ids.is_subset(&durable_child_ids),
        "live terminal must belong to the durable fixed group"
    );
    for child_run_id in &durable_child_ids {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!(
                        "/chat/runs/{child_run_id}/stream?last_index=0&replay_only=true"
                    ))
                    .header("authorization", auth.as_str())
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .expect("replay exact cancelled child");
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let replay = parse_sse_events(&String::from_utf8_lossy(&bytes));
        let terminal: Vec<_> = replay
            .iter()
            .filter(|event| event["type"] == "run_finished")
            .collect();
        assert_eq!(
            terminal.len(),
            1,
            "one durable cancellation receipt for {child_run_id}: {replay:?}"
        );
        assert_eq!(terminal[0]["run_id"], child_run_id.as_str());
        assert_eq!(terminal[0]["status"], "cancelled");
        assert_eq!(terminal[0]["cancellation_origin"], "user");
        assert!(!replay.iter().any(|event| {
            event["type"] == "text_done"
                && event["full_text"]
                    .as_str()
                    .is_some_and(|text| text.contains("late child output must be suppressed"))
        }));
    }
    let orphan_transcripts: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM session_transcript_items AS transcript \
         LEFT JOIN agent_runs AS run \
           ON run.user_id = transcript.user_id \
          AND run.session_id = transcript.session_id \
          AND run.run_id = transcript.run_id \
         WHERE transcript.user_id = ? AND transcript.session_id = ? \
           AND transcript.run_id IS NOT NULL AND run.run_id IS NULL",
    )
    .bind(&user_id)
    .bind(&session_id)
    .fetch_one(&ctx.pool)
    .await
    .expect("count cancelled fanout orphan transcript identities");
    assert_eq!(orphan_transcripts, 0);

    cleanup_session_data(&ctx.shared_pool, &user_id, &session_id).await;
    ctx.close().await;
}

/// A canonical Work graph is not decorative: after the root creates it, a
/// text-only root response must not be able to end the turn while either of
/// two ordered tasks is ready. This crosses the current-source HTTP server, MatrixOne Work graph,
/// internal scheduler, primary-session attempt, settlement, root transcript,
/// and final SSE projection. No child model loop is needed merely because a
/// task was declared.
pub async fn run_stream_canonical_work_scheduler_prevents_decorative_plan() {
    let b = bootstrap().await;
    let ctx = &b.ctx;
    let app = &ctx.app;
    let auth = &b.auth_header;
    let pool = &ctx.pool;
    let user_id = &ctx.user_id;

    // This journey owns no extra session: the bootstrap fixture already gives
    // it a fresh, owner-scoped session, and `ctx.close()` is its sole cleanup
    // owner. The scheduler contract is independent of creating a second
    // session, while a second lifecycle owner can race fixture teardown.
    let session_id = ctx.session_id.clone();

    let premature_reply = "Both tasks are already complete before the first attempt ran.";
    let second_premature_reply = "The second task is complete before its attempt ran.";
    let final_reply = "Both canonical tasks settled, and the verified result is ready.";
    let fixture_model = format!("mock-{}", ctx.suffix);
    ctx.install_native_provider(auth,vec![ProviderScript::new("run_stream_canonical_work_scheduler_prevents_decorative_plan",move |request|request.path=="/v1/chat/completions" && request.body["model"]==fixture_model && request.body["stream"]==true && request.body["messages"].as_array().is_some_and(|messages| messages.iter().any(|message|message["role"]=="user" && message["content"]=="Track this as one durable task and complete it.")),vec![ProviderResponse::OpenAi(json!({"choices":[{"index":0,"message":{"role":"assistant","content":"","reasoning_content":"","tool_calls":[provider_tool_call(
                            "create-canonical-work",
                            "start_work",
                            json!({
                                "goal": "Produce two ordered durable results",
                                "activation": "start",
                                "tasks": [
                                    {
                                        "objective": "Prepare the bounded result",
                                        "expected_result": "One durable prepared result"
                                    },
                                    {
                                        "objective": "Verify the prepared result",
                                        "expected_result": "One durable verified result"
                                    }
                                ]
                            })
                        )]},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":42,"completion_tokens":7,"total_tokens":49}})),ProviderResponse::OpenAi(json!({"choices":[{"index":0,"message":{"role":"assistant","content":premature_reply,"reasoning_content":"","tool_calls":[]},"finish_reason":"stop"}],"usage":{"prompt_tokens":42,"completion_tokens":7,"total_tokens":49}})),ProviderResponse::OpenAi(json!({"choices":[{"index":0,"message":{"role":"assistant","content":"","reasoning_content":"","tool_calls":[provider_tool_call(
                            "prepare-canonical-result",
                            "introspect",
                            json!({"facet":"session"})
                        )]},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":42,"completion_tokens":7,"total_tokens":49}})),ProviderResponse::OpenAi(json!({"choices":[{"index":0,"message":{"role":"assistant","content":"","reasoning_content":"","tool_calls":[provider_tool_call(
                            "settle-canonical-task",
                            "settle_work_item",
                            json!({
                                "outcome": "delivered",
                                "summary": "The primary session prepared the durable result."
                            })
                        )]},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":42,"completion_tokens":7,"total_tokens":49}})),ProviderResponse::OpenAi(json!({"choices":[{"index":0,"message":{"role":"assistant","content":second_premature_reply,"reasoning_content":"","tool_calls":[]},"finish_reason":"stop"}],"usage":{"prompt_tokens":42,"completion_tokens":7,"total_tokens":49}})),ProviderResponse::OpenAi(json!({"choices":[{"index":0,"message":{"role":"assistant","content":"","reasoning_content":"","tool_calls":[provider_tool_call(
                            "verify-canonical-result",
                            "introspect",
                            json!({"facet":"overview"})
                        )]},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":42,"completion_tokens":7,"total_tokens":49}})),ProviderResponse::OpenAi(json!({"choices":[{"index":0,"message":{"role":"assistant","content":"","reasoning_content":"","tool_calls":[provider_tool_call(
                            "settle-canonical-verification",
                            "settle_work_item",
                            json!({
                                "outcome": "delivered",
                                "summary": "The primary session verified the durable result."
                            })
                        )]},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":42,"completion_tokens":7,"total_tokens":49}})),ProviderResponse::OpenAi(json!({"choices":[{"index":0,"message":{"role":"assistant","content":final_reply,"reasoning_content":"","tool_calls":[]},"finish_reason":"stop"}],"usage":{"prompt_tokens":42,"completion_tokens":7,"total_tokens":49}}))])]).await;
    let (status, raw_sse) = stream_chat_full(
        app,
        auth,
        json!({
            "message": "Track this as one durable task and complete it.",
        "execution_policy":{"turn_intent":"fixed_default","skill_auto_route":"disabled"},
            "session_id": &session_id,
            "model_selection": seeded_model_selection(ctx),
            "context": {

            }
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "chat/stream: {raw_sse}");
    let events = parse_sse_events(&raw_sse);
    let start_receipt = events
        .iter()
        .find(|event| {
            event["type"].as_str() == Some("tool_call_end")
                && event["call_id"].as_str() == Some("create-canonical-work")
        })
        .and_then(|event| event["result"].as_str())
        .and_then(|result| serde_json::from_str::<Value>(result).ok())
        .unwrap_or_else(|| panic!("missing canonical Work start receipt: {raw_sse}"));
    let declared_tasks = start_receipt["declared_tasks"]
        .as_array()
        .unwrap_or_else(|| panic!("start receipt omitted task identities: {raw_sse}"));
    assert_eq!(
        declared_tasks.len(),
        2,
        "the server must issue one identity receipt per semantic task: {raw_sse}"
    );
    let declared_ids = declared_tasks
        .iter()
        .map(|task| {
            assert_eq!(task["item_revision"], 1, "initial receipt: {raw_sse}");
            task["item_id"]
                .as_str()
                .filter(|id| !id.trim().is_empty())
                .unwrap_or_else(|| panic!("server issued an empty task identity: {raw_sse}"))
        })
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(
        declared_ids.len(),
        declared_tasks.len(),
        "the server must issue distinct task identities: {raw_sse}"
    );
    let initial_board = &start_receipt["task_board_update"];
    assert_eq!(
        initial_board["schema_version"], 1,
        "the start receipt must carry a versioned live-board snapshot: {raw_sse}"
    );
    assert_eq!(
        initial_board["kind"], "snapshot",
        "start receipt: {raw_sse}"
    );
    let initial_board_tasks = initial_board["tasks"]
        .as_array()
        .unwrap_or_else(|| panic!("start receipt omitted live-board tasks: {raw_sse}"));
    // The canonical board snapshot includes its synthetic root alongside the
    // declared task items. Validate by item identity rather than array
    // position so the lifecycle projection can evolve without weakening the
    // two task obligations.
    assert_eq!(
        initial_board_tasks.len(),
        declared_tasks.len() + 1,
        "start receipt must include the canonical root plus declared tasks: {raw_sse}"
    );
    let board_task = |item_id: &str| {
        initial_board_tasks
            .iter()
            .find(|task| task["item_id"].as_str() == Some(item_id))
            .unwrap_or_else(|| panic!("missing board item {item_id}: {raw_sse}"))
    };
    let root_board_task = board_task("root");
    assert_eq!(root_board_task["execution_status"], "not_started");
    assert_eq!(root_board_task["delivery_status"], "unreported");
    let first_declared_id = declared_tasks[0]["item_id"]
        .as_str()
        .expect("first declared task identity");
    let second_declared_id = declared_tasks[1]["item_id"]
        .as_str()
        .expect("second declared task identity");
    let first_board_task = board_task(first_declared_id);
    let second_board_task = board_task(second_declared_id);
    assert_eq!(
        first_board_task["execution_status"], "running",
        "the first durable task must be visible as active before the next model round: {raw_sse}"
    );
    assert_eq!(first_board_task["delivery_status"], "unreported");
    assert_eq!(
        second_board_task["execution_status"], "not_started",
        "the second durable task must be visible immediately, not discovered by a later poll: {raw_sse}"
    );
    assert_eq!(second_board_task["delivery_status"], "unreported");
    let first_settlement = events
        .iter()
        .find(|event| {
            event["type"].as_str() == Some("tool_call_end")
                && event["call_id"].as_str() == Some("settle-canonical-task")
        })
        .and_then(|event| event["result"].as_str())
        .and_then(|result| serde_json::from_str::<Value>(result).ok())
        .unwrap_or_else(|| panic!("missing first canonical Work settlement receipt: {raw_sse}"));
    let first_delta = &first_settlement["task_board_update"];
    assert_eq!(first_delta["kind"], "upsert", "first settlement: {raw_sse}");
    let first_delta_tasks = first_delta["tasks"]
        .as_array()
        .unwrap_or_else(|| panic!("first settlement omitted live-board transitions: {raw_sse}"));
    assert_eq!(
        first_delta_tasks.len(),
        declared_tasks.len() + 1,
        "settlement must publish the complete canonical board: {raw_sse}"
    );
    assert_eq!(first_delta["work_id"], initial_board["work_id"]);
    assert_eq!(first_delta["branch_id"], initial_board["branch_id"]);
    let settled_board_task = |item_id: &str| {
        first_delta_tasks
            .iter()
            .find(|task| task["item_id"].as_str() == Some(item_id))
            .unwrap_or_else(|| panic!("settlement omitted board item {item_id}: {raw_sse}"))
    };
    assert_eq!(
        settled_board_task("root")["execution_status"],
        "not_started"
    );
    assert_eq!(settled_board_task("root")["delivery_status"], "unreported");
    let settled_first = settled_board_task(first_declared_id);
    assert_eq!(settled_first["execution_status"], "completed");
    assert_eq!(settled_first["delivery_status"], "delivered");
    let settled_second = settled_board_task(second_declared_id);
    assert_eq!(
        settled_second["execution_status"], "running",
        "the successor must become visible in the same durable settlement receipt: {raw_sse}"
    );
    assert_eq!(settled_second["delivery_status"], "unreported");
    let runnable_items = start_receipt["runnable_items"]
        .as_array()
        .unwrap_or_else(|| panic!("start receipt omitted runnable tasks: {raw_sse}"));
    assert_eq!(
        runnable_items,
        &declared_tasks[..1],
        "ordered task lists must expose only their first server-issued task as initially runnable: {raw_sse}"
    );
    let visible_texts = events
        .iter()
        .filter(|event| event["type"].as_str() == Some("text_delta"))
        .filter_map(|event| event["content"].as_str())
        .collect::<Vec<_>>();
    assert!(
        !visible_texts.contains(&premature_reply),
        "root text cannot claim delivery while the durable queue is ready: {raw_sse}"
    );
    assert!(
        !visible_texts.contains(&second_premature_reply),
        "root text cannot skip the second dependency-ready task: {raw_sse}"
    );
    assert!(
        visible_texts.contains(&final_reply),
        "only the post-settlement root synthesis may reach the user: {raw_sse}"
    );
    let scheduler_position = events
        .iter()
        .position(|event| {
            event["type"].as_str() == Some("tool_call")
                && event["tool_call"]["function"]["name"].as_str() == Some("run_next_work_item")
        })
        .unwrap_or_else(|| panic!("missing internal Work scheduler dispatch: {raw_sse}"));
    let final_position = events
        .iter()
        .position(|event| {
            event["type"].as_str() == Some("text_delta")
                && event["content"].as_str() == Some(final_reply)
        })
        .expect("final post-settlement root synthesis");
    assert!(
        scheduler_position < final_position,
        "scheduler dispatch must precede the final root synthesis: {raw_sse}"
    );

    let task_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM work_item_revisions r \
         JOIN work_branches b ON b.owner_id = r.owner_id AND b.work_id = r.work_id \
         WHERE r.owner_id = ? AND b.session_id = ? \
           AND r.item_kind = 'task' AND r.declaration_state = 'active'",
    )
    .bind(user_id)
    .bind(&session_id)
    .fetch_one(pool)
    .await
    .expect("count canonical task revisions");
    assert_eq!(task_count, 2, "both tasks must be durably declared");
    let settlement_rows = sqlx::query(
        "SELECT s.outcome, s.summary_text, s.status \
         FROM work_item_attempts s \
         JOIN work_branches b \
           ON b.owner_id = s.owner_id AND b.work_id = s.work_id AND b.branch_id = s.branch_id \
         WHERE s.owner_id = ? AND b.session_id = ?",
    )
    .bind(user_id)
    .bind(&session_id)
    .fetch_all(pool)
    .await
    .expect("load canonical task settlement");
    assert_eq!(
        settlement_rows.len(),
        2,
        "each task must settle exactly once"
    );
    assert!(
        settlement_rows.iter().all(|row| {
            row.get::<String, _>("outcome") == "delivered"
                && row.get::<String, _>("status") == "completed"
                && row
                    .get::<String, _>("summary_text")
                    .contains("durable result")
        }),
        "primary-session settlement must be persisted rather than inferred from root prose"
    );
    let execution_carriers: i64 = sqlx::query_scalar(
        "SELECT COUNT(DISTINCT s.executor_run_id) FROM work_item_attempts s
         JOIN work_branches b
           ON b.owner_id = s.owner_id AND b.work_id = s.work_id AND b.branch_id = s.branch_id
         WHERE s.owner_id = ? AND b.session_id = ?",
    )
    .bind(user_id)
    .bind(&session_id)
    .fetch_one(pool)
    .await
    .expect("count attempt execution carriers");
    assert_eq!(
        execution_carriers, 1,
        "ordered task attempts should stay in one primary model loop"
    );
    let child_runs: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM agent_runs
         WHERE user_id = ? AND session_id = ? AND parent_run_id IS NOT NULL",
    )
    .bind(user_id)
    .bind(&session_id)
    .fetch_one(pool)
    .await
    .expect("count child runs");
    assert_eq!(
        child_runs, 0,
        "declaring and completing ordered tasks must not construct a child model loop"
    );

    ctx.close().await;
}

/// A visible plan is not consent to execute it. A typed deferred activation
/// persists the graph without creating an attempt or letting the host's
/// completion guard synthesize a hidden dispatch.
pub async fn run_stream_deferred_work_does_not_start_an_attempt() {
    let b = bootstrap().await;
    let ctx = &b.ctx;
    let app = &ctx.app;
    let auth = &b.auth_header;
    let pool = &ctx.pool;
    let user_id = &ctx.user_id;

    let (status, session) = post_json(
        app,
        "/sessions",
        Some(auth.as_str()),
        json!({
            "title": "deferred canonical Work online gate",
            "metadata": {"suite": "canonical_work_deferred"}
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "create session: {session}");
    let session_id = session["session_id"]
        .as_str()
        .expect("session_id")
        .to_string();
    let final_reply = "The durable plan is ready; execution has not started.";
    let fixture_model = format!("mock-{}", ctx.suffix);
    ctx.install_native_provider(auth,vec![ProviderScript::new("run_stream_deferred_work_does_not_start_an_attempt",move |request|request.path=="/v1/chat/completions" && request.body["model"]==fixture_model && request.body["stream"]==true && request.body["messages"].as_array().is_some_and(|messages| messages.iter().any(|message|message["role"]=="user" && message["content"]=="Create a visible plan, but do not begin execution yet.")),vec![ProviderResponse::OpenAi(json!({"choices":[{"index":0,"message":{"role":"assistant","content":"","reasoning_content":"","tool_calls":[provider_tool_call(
                            "create-deferred-work",
                            "start_work",
                            json!({
                                "goal": "Prepare two bounded evidence tracks",
                                "activation": "defer",
                                "tasks": [
                                    {
                                        "objective": "Inspect the public surface",
                                        "expected_result": "One cited surface finding"
                                    },
                                    {
                                        "objective": "Inspect the direct route",
                                        "expected_result": "One cited route finding"
                                    }
                                ]
                            })
                        )]},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":42,"completion_tokens":7,"total_tokens":49}})),ProviderResponse::OpenAi(json!({"choices":[{"index":0,"message":{"role":"assistant","content":final_reply,"reasoning_content":"","tool_calls":[]},"finish_reason":"stop"}],"usage":{"prompt_tokens":42,"completion_tokens":7,"total_tokens":49}}))])]).await;
    let (status, raw_sse) = stream_chat_full(
        app,
        auth,
        json!({
            "message": "Create a visible plan, but do not begin execution yet.",
        "execution_policy":{"turn_intent":"fixed_default","skill_auto_route":"disabled"},
            "session_id": &session_id,
            "model_selection": seeded_model_selection(ctx),
            "context": {

            }
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "chat/stream: {raw_sse}");
    let events = parse_sse_events(&raw_sse);
    assert!(
        events.iter().any(|event| {
            event["type"].as_str() == Some("text_delta")
                && event["content"].as_str() == Some(final_reply)
        }),
        "deferred Work must allow the truthful acknowledgement to finish: {raw_sse}"
    );
    assert!(
        events.iter().all(|event| {
            event["type"].as_str() != Some("tool_call")
                || event["tool_call"]["function"]["name"].as_str() != Some("run_next_work_item")
        }),
        "deferred Work must not synthesize an execution dispatch: {raw_sse}"
    );
    let attempt_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM work_item_attempts a
         JOIN work_branches b
           ON b.owner_id = a.owner_id AND b.work_id = a.work_id AND b.branch_id = a.branch_id
         WHERE a.owner_id = ? AND b.session_id = ?",
    )
    .bind(user_id)
    .bind(&session_id)
    .fetch_one(pool)
    .await
    .expect("count deferred Work attempts");
    assert_eq!(attempt_count, 0, "deferred Work owns no execution attempt");
    let ready_task_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM work_item_revisions r
         JOIN work_branches b ON b.owner_id = r.owner_id AND b.work_id = r.work_id
         WHERE r.owner_id = ? AND b.session_id = ?
           AND r.item_kind = 'task' AND r.declaration_state = 'active'",
    )
    .bind(user_id)
    .bind(&session_id)
    .fetch_one(pool)
    .await
    .expect("count deferred Work tasks");
    assert_eq!(ready_task_count, 2, "the deferred graph remains durable");

    cleanup_session_data(&ctx.shared_pool, user_id, &session_id).await;
    ctx.close().await;
}

/// Unhappy-path companion to the structured fan-in gate. Provider failure in
/// every child remains one fixed-size terminal group: no replacement agents,
/// no per-child parent analysis, and one synthesis over the preserved causes.
pub async fn run_stream_failed_fanout_settles_once_without_orphaning_children() {
    let b = bootstrap().await;
    let ctx = &b.ctx;
    let app = &ctx.app;
    let auth = &b.auth_header;
    let pool = &ctx.pool;
    let user_id = &ctx.user_id;

    let (status, session) = post_json(
        app,
        "/sessions",
        Some(auth.as_str()),
        json!({
            "title": "failed structured fan-in online gate",
            "metadata": {"suite": "structured_fanout_failure_online", "full_llm_capture": true}
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "create session: {session}");
    let session_id = session["session_id"]
        .as_str()
        .expect("session_id")
        .to_string();
    let final_reply = "One parent synthesis disclosed all three failed child causes.";
    let fixture_model = format!("mock-{}", ctx.suffix);
    let releases =
        ReleaseProviderGatesOnDrop(vec![std::sync::Arc::new(tokio::sync::Notify::new())]);
    let root_model = fixture_model.clone();
    let child_model = fixture_model.clone();
    ctx.install_native_provider(auth,vec![
        ProviderScript::new("actual parent fanout execution",move |request| request.path=="/v1/chat/completions" && request.body["model"]==root_model && request.body["stream"]==true && delegation_assessment(&request.body).is_none() && request.body["messages"].as_array().is_some_and(|messages|messages.iter().any(|message|message["role"]=="user" && message["content"]=="Run three reviews and preserve every failure cause.")),vec![
                    native_search_response("online-failed-fanout-search", "agent_fanout"),
                    native_invoke_response(
                        "online-failed-fanout-start",
                        "agent_fanout",
                        json!({
                            "action": "start",
                            "group_id": "online-failed-review-group",
                            "title": "Online failed three-way review",
                            "target_count": 3,
                            "slots": [
                                {"id": "storage", "description": "Failed storage review", "prompt": "Inspect storage."},
                                {"id": "runtime", "description": "Failed runtime review", "prompt": "Inspect runtime."},
                                {"id": "journey", "description": "Failed journey review", "prompt": "Inspect journey."}
                            ],
                            "defaults": {"agent_type": "general-purpose"}
                        }),
                    ),
                    gated_native_delta(
                        json!({"tool_calls":[{"index":0,"id":"online-failed-fanout-wait","type":"function","function":{"name":"invoke_tool","arguments":json!({"name":"agent","arguments":{"action":"wait","timeout_ms":10000}}).to_string()}}]}),
                        "tool_calls", releases.0[0].clone(),
                    ), native_text_response(final_reply)]),
        native_child_script(child_model.clone(),"Run three reviews and preserve every failure cause.","Inspect storage.",ProviderResponse::Json {status:StatusCode::BAD_REQUEST,body:json!({"error":{"message":"private failed child marker"}})}),
native_child_script(child_model.clone(),"Run three reviews and preserve every failure cause.","Inspect runtime.",ProviderResponse::Json {status:StatusCode::BAD_REQUEST,body:json!({"error":{"message":"private failed child marker"}})}),
native_child_script(child_model.clone(),"Run three reviews and preserve every failure cause.","Inspect journey.",ProviderResponse::Json {status:StatusCode::BAD_REQUEST,body:json!({"error":{"message":"private failed child marker"}})}),
    ]).await;
    let payload = json!({
        "message": "Run three reviews and preserve every failure cause.",
        "execution_policy": {"turn_intent":"fixed_default","skill_auto_route":"disabled"},
        "session_id": &session_id,
        "model_selection": seeded_model_selection(ctx),
    });
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(30);
    let response = tokio::time::timeout_at(
        deadline,
        app.clone().oneshot(
            Request::builder()
                .method("POST")
                .uri("/chat/stream")
                .header("authorization", auth.as_str())
                .header("content-type", "application/json")
                .body(Body::from(payload.to_string()))
                .unwrap(),
        ),
    )
    .await
    .expect("bounded stream admission")
    .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let mut stream = response.into_body().into_data_stream();
    let mut bytes = Vec::new();
    // Independent child failures can arrive across provider rounds. Hold the
    // third response until all real terminals arrive so this finite script
    // tests one synthesis over the whole group, not scheduler timing. An
    // agent wait is an input observation, not an all-children completion fence.
    loop {
        let chunk = tokio::time::timeout_at(deadline, stream.next())
            .await
            .expect("bounded child failure delivery")
            .expect("live stream before child failures")
            .unwrap();
        bytes.extend_from_slice(&chunk);
        if parse_sse_events(&String::from_utf8_lossy(&bytes))
            .iter()
            .filter(|event| event["type"] == "agent_failed")
            .count()
            == 3
        {
            break;
        }
    }
    releases.0[0].notify_one();
    while let Some(chunk) = tokio::time::timeout_at(deadline, stream.next())
        .await
        .expect("bounded parent synthesis")
    {
        bytes.extend_from_slice(&chunk.unwrap());
    }
    let raw_sse = String::from_utf8(bytes).unwrap();
    assert_native_delegation_without_auxiliary_selector(
        ctx,
        "Run three reviews and preserve every failure cause.",
        &[
            ("Failed storage review", "Inspect storage."),
            ("Failed runtime review", "Inspect runtime."),
            ("Failed journey review", "Inspect journey."),
        ],
    )
    .await;
    let events = parse_sse_events(&raw_sse);
    let root_run_id = events
        .iter()
        .find(|event| event["type"].as_str() == Some("session_info"))
        .and_then(|event| event["run_id"].as_str())
        .expect("root run id")
        .to_string();
    let failed_positions = events
        .iter()
        .enumerate()
        .filter(|(_, event)| event["type"].as_str() == Some("agent_failed"))
        .map(|(index, _)| index)
        .collect::<Vec<_>>();
    assert_eq!(
        failed_positions.len(),
        3,
        "each failed child gets one terminal projection: {raw_sse}"
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| {
                event["type"].as_str() == Some("agent_live_event")
                    && event["event_kind"].as_str() == Some("agent_terminated")
                    && event["termination"].as_str() == Some("failed")
            })
            .count(),
        3,
        "the workbench live lane also gets one failed terminal per child"
    );
    let final_positions = events
        .iter()
        .enumerate()
        .filter(|(_, event)| {
            event["type"].as_str() == Some("text_delta")
                && event["content"].as_str() == Some(final_reply)
        })
        .map(|(index, _)| index)
        .collect::<Vec<_>>();
    assert_eq!(
        final_positions.len(),
        1,
        "one failed-group synthesis: {raw_sse}"
    );
    assert!(
        failed_positions
            .iter()
            .all(|position| *position < final_positions[0]),
        "parent analyzed before the failed group settled: {raw_sse}"
    );
    let aggregate = events
        .iter()
        .find(|event| {
            event["type"].as_str() == Some("tool_call_end")
                && event["call_id"].as_str() == Some("online-failed-fanout-start")
        })
        .and_then(|event| event["result"].as_str())
        .and_then(|result| serde_json::from_str::<Value>(result).ok())
        .unwrap_or_else(|| panic!("missing failed fanout aggregate: {raw_sse}"));
    assert_eq!(aggregate["target_count"], 3, "{aggregate}");
    assert_eq!(aggregate["status"], "started");
    let launched = aggregate["agents"].as_array().unwrap();
    assert_eq!(launched.len(), 3);
    assert_eq!(
        launched
            .iter()
            .map(|child| child["slot_index"].as_u64().unwrap())
            .collect::<Vec<_>>(),
        vec![0, 1, 2]
    );
    let children = captured_synthesis_children(
        pool,
        user_id,
        &session_id,
        &root_run_id,
        final_reply,
        &events,
    )
    .await;
    assert_eq!(children.len(), 3);
    assert!(children.iter().all(|child| child["status"] == "failed"));
    assert_eq!(
        children
            .iter()
            .map(|child| (
                child["agent_id"].as_str().unwrap(),
                child["run_id"].as_str().unwrap()
            ))
            .collect::<std::collections::HashSet<_>>(),
        launched
            .iter()
            .map(|child| (
                child["agent_id"].as_str().unwrap(),
                child["run_id"].as_str().unwrap()
            ))
            .collect::<std::collections::HashSet<_>>()
    );
    assert!(
        children.iter().all(|child| child["result"]
            .as_str()
            .unwrap()
            .contains("[invalid_request] LLM request rejected: 400")),
        "failure provenance was lost: {children:?}"
    );

    assert!(!raw_sse.contains("private failed child marker"));
    assert!(
        !json!(children)
            .to_string()
            .contains("private failed child marker")
    );
    let durable_deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(20);
    loop {
        let terminal: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM agent_runs WHERE user_id = ? AND session_id = ? \
             AND status IN ('completed', 'failed')",
        )
        .bind(user_id)
        .bind(&session_id)
        .fetch_one(pool)
        .await
        .unwrap_or(0);
        if terminal == 4 {
            break;
        }
        assert!(
            tokio::time::Instant::now() < durable_deadline,
            "failed run tree did not settle: {terminal}/4"
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    let runs = sqlx::query(
        "SELECT run_id, parent_run_id, root_run_id, depth, status FROM agent_runs \
         WHERE user_id = ? AND session_id = ? ORDER BY depth ASC, run_id ASC",
    )
    .bind(user_id)
    .bind(&session_id)
    .fetch_all(pool)
    .await
    .expect("failed durable run tree");
    assert_eq!(runs.len(), 4, "{runs:?}");
    assert!(
        runs.iter()
            .filter(|run| run.get::<i32, _>("depth") == 1)
            .all(|run| {
                run.get::<String, _>("status") == "failed"
                    && run.get::<Option<String>, _>("parent_run_id").as_deref()
                        == Some(root_run_id.as_str())
                    && run.get::<String, _>("root_run_id") == root_run_id
            })
    );
    let reconciliations: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM agent_events WHERE user_id = ? AND session_id = ? \
         AND event_type = 'runtime_reconciliation'",
    )
    .bind(user_id)
    .bind(&session_id)
    .fetch_one(pool)
    .await
    .expect("count failure reconciliations");
    assert_eq!(reconciliations, 0);

    cleanup_session_data(&ctx.shared_pool, user_id, &session_id).await;
    ctx.close().await;
}

/// B2: Core events (user_query, llm_response) and context_trace_signal are persisted
/// to agent_events after stream_chat completes.
pub async fn run_stream_context_trace_persistence() {
    let b = bootstrap().await;
    let ctx = &b.ctx;
    let app = &ctx.app;
    let auth = &b.auth_header;
    let pool = &ctx.pool;
    let user_id = &ctx.user_id;

    let (st_sess, sess) = post_json(
        app,
        "/sessions",
        Some(auth.as_str()),
        json!({ "title": "phase-b trace signal", "metadata": { "suite": "ctx_trace" } }),
    )
    .await;
    assert_eq!(st_sess, StatusCode::CREATED, "create session: {sess}");
    let session_id = sess["session_id"].as_str().expect("session_id").to_string();

    let fixture_model = format!("mock-{}", ctx.suffix);
    ctx.install_native_provider(auth,vec![ProviderScript::new("run_stream_context_trace_persistence",move |request| request.path=="/v1/chat/completions" && request.body["model"]==fixture_model && request.body["stream"]==true && request.body["messages"].as_array().is_some_and(|messages| messages.iter().any(|message|message["role"]=="user" && message["content"]=="context trace persistence test")),vec![ProviderResponse::OpenAi(json!({"choices":[{"index":0,"message":{"role":"assistant","content":"Context trace reply.","reasoning_content":"","tool_calls":[]},"finish_reason":"stop"}],"usage":{ "prompt_tokens": 100, "completion_tokens": 50,
                    "prompt_tokens_details": {"cached_tokens": 0, "cache_creation_input_tokens": 0} }}))])]).await;
    let payload = json!({
        "message": "context trace persistence test",
    "execution_policy":{"turn_intent":"fixed_default","skill_auto_route":"disabled"},
        "session_id": &session_id,
        "model_selection": seeded_model_selection(ctx),
        "context": {

        }
    });
    let (status, body) = stream_chat_full(app, auth, payload).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "chat/stream: {}",
        &body[..body.len().min(300)]
    );

    // Wait for events to be persisted (user_query + llm_response + context_trace_signal = 3).
    wait_for_agent_events_count(
        pool,
        user_id,
        &session_id,
        3,
        std::time::Duration::from_secs(15),
    )
    .await;

    let recs = sqlx::query(
        "SELECT event_id, event_type, content, causal_chain_id, agent_id, \
                llm_model_used, CAST(token_usage AS CHAR) AS token_usage, parent_event_id \
         FROM agent_events WHERE user_id = ? AND session_id = ? ORDER BY created_at ASC",
    )
    .bind(user_id)
    .bind(&session_id)
    .fetch_all(pool)
    .await
    .expect("select agent_events");

    assert!(
        recs.len() >= 3,
        "should have at least 3 agent_events rows (user_query + llm_response + context_trace_signal), got {}",
        recs.len()
    );

    // ── Verify user_query event ──
    let user_query = recs
        .iter()
        .find(|r| r.try_get::<String, _>("event_type").ok().as_deref() == Some("user_query"));
    assert!(
        user_query.is_some(),
        "should have a user_query event. Found types: {:?}",
        recs.iter()
            .filter_map(|r| r.try_get::<String, _>("event_type").ok())
            .collect::<Vec<_>>()
    );
    let uq = user_query.unwrap();
    let uq_content = uq.try_get::<String, _>("content").unwrap_or_default();
    assert!(
        uq_content.contains("context trace persistence test"),
        "user_query content should contain the original message, got: {uq_content}"
    );
    let uq_event_id = uq.try_get::<String, _>("event_id").unwrap_or_default();
    assert!(
        !uq_event_id.is_empty(),
        "user_query should have an event_id"
    );
    let uq_chain_id = uq
        .try_get::<Option<String>, _>("causal_chain_id")
        .ok()
        .flatten()
        .unwrap_or_default();
    assert!(
        uq_chain_id.contains("server-loop"),
        "user_query causal_chain_id should contain 'server-loop', got: {uq_chain_id}"
    );

    // ── Verify llm_response event ──
    let llm_response = recs
        .iter()
        .find(|r| r.try_get::<String, _>("event_type").ok().as_deref() == Some("llm_response"));
    assert!(
        llm_response.is_some(),
        "should have an llm_response event. Found types: {:?}",
        recs.iter()
            .filter_map(|r| r.try_get::<String, _>("event_type").ok())
            .collect::<Vec<_>>()
    );
    let lr = llm_response.unwrap();
    let lr_content = lr.try_get::<String, _>("content").unwrap_or_default();
    assert!(
        lr_content.contains("Context trace reply."),
        "llm_response content should contain LLM text, got: {lr_content}"
    );
    let usage: Value = serde_json::from_str(
        &lr.try_get::<String, _>("token_usage")
            .expect("canonical usage"),
    )
    .expect("usage JSON");
    assert_eq!(usage["input_tokens"], 100);
    assert_eq!(usage["output_tokens"], 50);

    let (restore_status, restored) = post_json(
        app,
        &format!("/sessions/{session_id}/resume"),
        Some(auth.as_str()),
        json!({}),
    )
    .await;
    assert_eq!(restore_status, StatusCode::OK);
    let restored: astra_services::session_restore::RestoredSession =
        serde_json::from_value(restored).expect("versioned session restore response");
    assert!(restored.resume_messages().iter().any(|message| {
        message["role"] == "assistant"
            && message["content"].as_str() == Some("Context trace reply.")
    }));
    for table in [
        "session_state_items",
        "session_state_item_events",
        "ctx_decision_audits",
        "skill_selection_events",
    ] {
        let count: i64 = sqlx::query_scalar(&format!(
            "SELECT COUNT(*) FROM {table} WHERE user_id = ? AND session_id = ?"
        ))
        .bind(user_id)
        .bind(&session_id)
        .fetch_one(pool)
        .await
        .expect("ordinary answer projection count");
        assert_eq!(count, 0, "ordinary answer must not create {table}");
    }
    let lr_chain_id = lr
        .try_get::<Option<String>, _>("causal_chain_id")
        .ok()
        .flatten()
        .unwrap_or_default();
    assert_eq!(
        uq_chain_id, lr_chain_id,
        "user_query and llm_response should share the same causal_chain_id"
    );
    // llm_response should have parent_event_id pointing to user_query.
    let lr_parent = lr
        .try_get::<Option<String>, _>("parent_event_id")
        .ok()
        .flatten()
        .unwrap_or_default();
    assert_eq!(
        lr_parent, uq_event_id,
        "llm_response parent_event_id should be user_query event_id"
    );

    // ── Verify context_trace_signal event ──
    let trace_signal = recs.iter().find(|r| {
        r.try_get::<String, _>("event_type").ok().as_deref() == Some("context_trace_signal")
    });
    assert!(
        trace_signal.is_some(),
        "should have a context_trace_signal event. Found types: {:?}",
        recs.iter()
            .filter_map(|r| r.try_get::<String, _>("event_type").ok())
            .collect::<Vec<_>>()
    );
    let trace = trace_signal.unwrap();
    let trace_chain_id = trace
        .try_get::<Option<String>, _>("causal_chain_id")
        .ok()
        .flatten()
        .unwrap_or_default();
    assert!(
        trace_chain_id.contains("context-trace"),
        "context_trace_signal causal_chain_id should contain 'context-trace', got: {trace_chain_id}"
    );

    // ── Cleanup ──
    cleanup_session_data(&ctx.shared_pool, user_id, &session_id).await;
    ctx.close().await;
}

/// B3: Multiple stream_chat calls to same session → core events + context traces per turn.
pub async fn run_stream_multi_turn_persistence() {
    let b = bootstrap().await;
    let ctx = &b.ctx;
    let app = &ctx.app;
    let auth = &b.auth_header;
    let pool = &ctx.pool;
    let user_id = &ctx.user_id;

    let (st_sess, sess) = post_json(
        app,
        "/sessions",
        Some(auth.as_str()),
        json!({ "title": "phase-b multi-turn", "metadata": { "suite": "multi_turn" } }),
    )
    .await;
    assert_eq!(st_sess, StatusCode::CREATED, "create session: {sess}");
    let session_id = sess["session_id"].as_str().expect("session_id").to_string();

    // ── Turn 1 ──
    let fixture_model = format!("mock-{}", ctx.suffix);
    ctx.install_native_provider(auth,vec![ProviderScript::new("run_stream_multi_turn_persistence",move |request| request.path=="/v1/chat/completions" && request.body["model"]==fixture_model && request.body["stream"]==true && request.body["messages"].as_array().is_some_and(|messages| messages.iter().any(|message|message["role"]=="user" && message["content"]=="multi-turn message one")),vec![ProviderResponse::OpenAi(json!({"choices":[{"index":0,"message":{"role":"assistant","content":"Response one.","reasoning_content":"","tool_calls":[]},"finish_reason":"stop"}],"usage":{"prompt_tokens":42,"completion_tokens":7,"total_tokens":49}}))])]).await;
    let payload1 = json!({
        "message": "multi-turn message one",
    "execution_policy":{"turn_intent":"fixed_default","skill_auto_route":"disabled"},
        "session_id": &session_id,
        "model_selection": seeded_model_selection(ctx),
        "context": {

        }
    });
    let (st1, body1) = stream_chat_full(app, auth, payload1).await;
    assert_eq!(
        st1,
        StatusCode::OK,
        "turn 1: {}",
        &body1[..body1.len().min(200)]
    );

    // Wait for Turn 1 events (user_query + llm_response + context_trace_signal = 3).
    wait_for_agent_events_count(
        pool,
        user_id,
        &session_id,
        3,
        std::time::Duration::from_secs(15),
    )
    .await;

    let count_after_turn1: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM agent_events WHERE user_id = ? AND session_id = ?",
    )
    .bind(user_id)
    .bind(&session_id)
    .fetch_one(pool)
    .await
    .unwrap_or(0);
    assert!(
        count_after_turn1 >= 3,
        "turn 1 should have >= 3 events (user_query + llm_response + context_trace_signal), got {count_after_turn1}"
    );

    // Verify turn 1 has user_query with the right content.
    let uq1_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM agent_events WHERE user_id = ? AND session_id = ? AND event_type = 'user_query'",
    )
    .bind(user_id)
    .bind(&session_id)
    .fetch_one(pool)
    .await
    .unwrap_or(0);
    assert_eq!(uq1_count, 1, "after turn 1: exactly 1 user_query event");

    // ── Turn 2 ──
    let fixture_model = format!("mock-{}", ctx.suffix);
    ctx.install_native_provider(auth,vec![ProviderScript::new("run_stream_multi_turn_persistence",move |request| request.path=="/v1/chat/completions" && request.body["model"]==fixture_model && request.body["stream"]==true && request.body["messages"].as_array().is_some_and(|messages| messages.iter().any(|message|message["role"]=="user" && message["content"]=="multi-turn message two")),vec![ProviderResponse::OpenAi(json!({"choices":[{"index":0,"message":{"role":"assistant","content":"Response two.","reasoning_content":"","tool_calls":[]},"finish_reason":"stop"}],"usage":{"prompt_tokens":42,"completion_tokens":7,"total_tokens":49}}))])]).await;
    let payload2 = json!({
        "message": "multi-turn message two",
    "execution_policy":{"turn_intent":"fixed_default","skill_auto_route":"disabled"},
        "session_id": &session_id,
        "model_selection": seeded_model_selection(ctx),
        "context": {

        }
    });
    let (st2, body2) = stream_chat_full(app, auth, payload2).await;
    assert_eq!(
        st2,
        StatusCode::OK,
        "turn 2: {}",
        &body2[..body2.len().min(200)]
    );

    // Wait for Turn 2 events (3 more events).
    wait_for_agent_events_count(
        pool,
        user_id,
        &session_id,
        count_after_turn1 + 3,
        std::time::Duration::from_secs(15),
    )
    .await;

    let count_after_turn2: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM agent_events WHERE user_id = ? AND session_id = ?",
    )
    .bind(user_id)
    .bind(&session_id)
    .fetch_one(pool)
    .await
    .unwrap_or(0);
    assert!(
        count_after_turn2 >= count_after_turn1 + 3,
        "turn 2 should add >= 3 events: after_turn1={count_after_turn1}, after_turn2={count_after_turn2}"
    );

    // ── Verify event types across both turns ──
    let uq_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM agent_events WHERE user_id = ? AND session_id = ? AND event_type = 'user_query'",
    )
    .bind(user_id)
    .bind(&session_id)
    .fetch_one(pool)
    .await
    .unwrap_or(0);
    assert_eq!(
        uq_count, 2,
        "should have 2 user_query events (one per turn)"
    );

    let lr_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM agent_events WHERE user_id = ? AND session_id = ? AND event_type = 'llm_response'",
    )
    .bind(user_id)
    .bind(&session_id)
    .fetch_one(pool)
    .await
    .unwrap_or(0);
    assert_eq!(
        lr_count, 2,
        "should have 2 llm_response events (one per turn)"
    );

    // Verify multiple causal_chain_ids — each turn's core events get a server-loop chain,
    // and each turn's context_trace gets a context-trace chain.
    let chains: Vec<String> = sqlx::query_scalar(
        "SELECT DISTINCT causal_chain_id FROM agent_events \
         WHERE user_id = ? AND session_id = ? AND causal_chain_id IS NOT NULL",
    )
    .bind(user_id)
    .bind(&session_id)
    .fetch_all(pool)
    .await
    .unwrap_or_default();
    let server_loop_chains = chains.iter().filter(|c| c.contains("server-loop")).count();
    let ctx_trace_chains = chains
        .iter()
        .filter(|c| c.contains("context-trace"))
        .count();
    assert!(
        server_loop_chains >= 2,
        "should have >= 2 server-loop causal chains (one per turn), got {server_loop_chains} in {:?}",
        chains
    );
    assert!(
        ctx_trace_chains >= 2,
        "should have >= 2 context-trace causal chains (one per turn), got {ctx_trace_chains} in {:?}",
        chains
    );

    // Both runs should be queryable.
    let si1 = sse_first_data_json_with_type(&body1, "session_info");
    let si2 = sse_first_data_json_with_type(&body2, "session_info");
    assert!(
        si1.is_some() && si2.is_some(),
        "both turns should have session_info"
    );
    let si1_val = si1.unwrap();
    let si2_val = si2.unwrap();
    let run_id_1 = si1_val["run_id"].as_str().unwrap();
    let run_id_2 = si2_val["run_id"].as_str().unwrap();
    assert_ne!(
        run_id_1, run_id_2,
        "each turn should create a distinct run_id"
    );

    // Verify both runs are completed.
    for (label, rid) in [("run1", run_id_1), ("run2", run_id_2)] {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let (st, rj) =
                get_json(app, &format!("/chat/runs/{rid}"), Some(auth.as_str()), &[]).await;
            assert_eq!(st, StatusCode::OK, "{label} status: {rj}");
            if rj["status"].as_str() == Some("completed") {
                break;
            }
            if tokio::time::Instant::now() >= deadline {
                panic!("{label} did not reach completed within 10s: {rj}");
            }
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        }
    }

    // ── Cleanup ──
    cleanup_session_data(&ctx.shared_pool, user_id, &session_id).await;
    ctx.close().await;
}
