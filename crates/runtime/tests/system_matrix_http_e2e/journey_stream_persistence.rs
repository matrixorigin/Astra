//! Phase B: `/chat/stream` (server-driven loop) persistence — verify session,
//! core events (`user_query`, `llm_response`), context trace events, and run
//! status in DB after web-agent mode chat.
//!
//! ## Architecture note
//!
//! The `/chat/stream` path uses the server-driven agentic loop (`stream_chat()`),
//! NOT the bridge-driven path. It persists:
//!
//! 1. **`user_query`** + **`llm_response`** events — via `persist_server_loop_core_events()`
//!    using `TurnCoreEventWriter` after the agentic loop completes
//! 2. **`context_trace_signal`** events — via `ContextTracePersistenceContext` during
//!    `finalize_turn_trace()` at loop exit
//! 3. **Run status + usage** — via `RunEngine` (when configured) to durable store
//! 4. **Promotion events** — via `persist_runtime_promotion_events()`
//!
//! These tests verify that infrastructure.

use axum::http::StatusCode;
use axum::{body::Body, http::Request};
use futures_util::StreamExt;
use serde_json::{Value, json};
use sqlx::Row;
use tower::util::ServiceExt;

use super::harness::{
    bootstrap, cleanup_session_data, get_json, post_json, seeded_selected_model,
    sse_first_data_json_with_type,
};

/// Collect the FULL SSE stream body (up to deadline), not just until session_info.
/// Returns (status, body_text).
async fn collect_full_sse_stream(
    app: &axum::Router,
    req: Request<Body>,
    timeout_secs: u64,
) -> (StatusCode, String) {
    let resp = app.clone().oneshot(req).await.expect("oneshot");
    let status = resp.status();
    let mut stream = resp.into_body().into_data_stream();
    let mut acc = Vec::new();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);
    while let Ok(Some(chunk)) = tokio::time::timeout_at(deadline, stream.next()).await {
        let chunk = chunk.expect("body chunk");
        acc.extend_from_slice(&chunk);
    }
    (status, String::from_utf8_lossy(&acc).into_owned())
}

/// Stream a chat, wait for the full stream to end, return (status, raw_body).
async fn stream_chat_full(app: &axum::Router, auth: &str, payload: Value) -> (StatusCode, String) {
    let test_secret = std::env::var("ASTRA_TEST_BRIDGE_SECRET").expect("bridge test secret");
    let req = Request::builder()
        .method("POST")
        .uri("/chat/stream")
        .header("authorization", auth)
        .header("content-type", "application/json")
        .header("x-mo-bridge-test-secret", &test_secret)
        .body(Body::from(payload.to_string()))
        .expect("stream request");
    collect_full_sse_stream(app, req, 30).await
}

fn mock_tool_call(id: &str, name: &str, args: Value) -> Value {
    json!({
        "id": id,
        "type": "function",
        "function": {
            "name": name,
            "arguments": args.to_string()
        }
    })
}

fn parse_sse_events(raw: &str) -> Vec<Value> {
    raw.lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .filter_map(|data| serde_json::from_str(data).ok())
        .collect()
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

    // ── Chat/stream with text-only mock LLM response ──
    // test_llm_rounds in context takes the stream_chat() server-loop path.
    let payload = json!({
        "message": "phase-b persistence probe",
        "session_id": &session_id,
        "selected_model": seeded_selected_model(ctx),
        "context": {
            "test_llm_rounds": [{ "full_text": "Persistence verified." }]
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
    ctx.pool.close().await;
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
    let payload = json!({
        "message": "Run three reviews as one structured work group.",
        "session_id": &session_id,
        "selected_model": seeded_selected_model(ctx),
        "context": {
            "test_llm_rounds": [
                {
                    "tool_calls": [mock_tool_call(
                        "online-fanout-start",
                        "agent_fanout",
                        fanout_args
                    )]
                },
                {"full_text": final_reply}
            ],
            "test_spawn_child_llm_rounds": [
                {"full_text": "durable child review result"}
            ]
        }
    });
    let (status, raw_sse) = stream_chat_full(app, auth, payload).await;
    assert_eq!(status, StatusCode::OK, "chat/stream: {raw_sse}");
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
    ctx.pool.close().await;
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
            "metadata": {"suite": "structured_fanout_failure_online"}
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "create session: {session}");
    let session_id = session["session_id"]
        .as_str()
        .expect("session_id")
        .to_string();
    let final_reply = "One parent synthesis disclosed all three failed child causes.";
    let (status, raw_sse) = stream_chat_full(
        app,
        auth,
        json!({
            "message": "Run three reviews and preserve every failure cause.",
            "session_id": &session_id,
            "selected_model": seeded_selected_model(ctx),
            "context": {
                "test_llm_rounds": [
                    {
                        "tool_calls": [mock_tool_call(
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
                            })
                        )]
                    },
                    {"full_text": final_reply}
                ],
                "test_spawn_child_llm_rounds": [{
                    "error": {
                        "message": "online child provider failed with preserved cause",
                        "kind": "server_error"
                    }
                }]
            }
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "chat/stream: {raw_sse}");
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
    assert_eq!(aggregate["completed"], 0, "{aggregate}");
    assert_eq!(aggregate["failed"], 3, "{aggregate}");
    assert!(
        aggregate
            .to_string()
            .contains("online child provider failed with preserved cause"),
        "failure provenance was lost: {aggregate}"
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
    ctx.pool.close().await;
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

    let payload = json!({
        "message": "context trace persistence test",
        "session_id": &session_id,
        "selected_model": seeded_selected_model(ctx),
        "context": {
            "test_llm_rounds": [{ "full_text": "Context trace reply." }]
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
                llm_model_used, token_usage, parent_event_id \
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
    ctx.pool.close().await;
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
    let payload1 = json!({
        "message": "multi-turn message one",
        "session_id": &session_id,
        "selected_model": seeded_selected_model(ctx),
        "context": {
            "test_llm_rounds": [{ "full_text": "Response one." }]
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
    let payload2 = json!({
        "message": "multi-turn message two",
        "session_id": &session_id,
        "selected_model": seeded_selected_model(ctx),
        "context": {
            "test_llm_rounds": [{ "full_text": "Response two." }]
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
    ctx.pool.close().await;
}
