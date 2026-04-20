//! Phase B: `/chat/stream` (server-driven loop) persistence — verify session,
//! context trace events, and run status in DB after web-agent mode chat.
//!
//! ## Architecture note
//!
//! The `/chat/stream` path uses the server-driven agentic loop (`stream_chat()`),
//! NOT the bridge-driven path. The bridge path (`/chat/turn`) persists `user_query`
//! and `llm_response` events to `agent_events`. The server loop path persists:
//!
//! 1. **`context_trace_signal`** events — via `ContextTracePersistenceContext` during
//!    `finalize_turn_trace()` at loop exit
//! 2. **Run status + usage** — via `RunEngine` (when configured) to durable store
//! 3. **Promotion events** — via `persist_runtime_promotion_events()`
//!
//! These tests verify that infrastructure.

use axum::http::StatusCode;
use axum::{body::Body, http::Request};
use futures_util::StreamExt;
use serde_json::{Value, json};
use sqlx::Row;
use tower::util::ServiceExt;

use super::harness::{
    bootstrap, cleanup_session_data, get_json, post_json, sse_first_data_json_with_type,
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
    (status, String::from_utf8_lossy(&acc).to_string())
}

/// Stream a chat, wait for the full stream to end, return (status, raw_body).
async fn stream_chat_full(app: &axum::Router, auth: &str, payload: Value) -> (StatusCode, String) {
    let test_secret = std::env::var("ASTRA_BRIDGE_TEST_SECRET").expect("bridge test secret");
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

/// Poll until `agent_events` has at least `min_count` rows for the session.
async fn wait_for_agent_events_count(
    pool: &sqlx::MySqlPool,
    session_id: &str,
    min_count: i64,
    timeout: std::time::Duration,
) {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM agent_events WHERE session_id = ?")
            .bind(session_id)
            .fetch_one(pool)
            .await
            .unwrap_or(0);
        if n >= min_count {
            return;
        }
        if tokio::time::Instant::now() >= deadline {
            panic!(
                "timeout ({timeout:?}) waiting for >= {min_count} agent_events for session_id={session_id} (got {n})"
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
    let row = sqlx::query("SELECT user_id, status FROM agent_sessions WHERE session_id = ?")
        .bind(&session_id)
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
    cleanup_session_data(pool, &session_id).await;
    ctx.pool.close().await;
}

/// B2: Context trace signal is persisted to agent_events after stream_chat completes.
pub async fn run_stream_context_trace_persistence() {
    let b = bootstrap().await;
    let ctx = &b.ctx;
    let app = &ctx.app;
    let auth = &b.auth_header;
    let pool = &ctx.pool;

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

    // Wait for context_trace_signal to be persisted.
    // The finalize_turn_trace → persist_latest_context_trace_signal path writes
    // this event type asynchronously after the loop completes.
    wait_for_agent_events_count(pool, &session_id, 1, std::time::Duration::from_secs(15)).await;

    let recs = sqlx::query(
        "SELECT event_id, event_type, content, causal_chain_id, agent_id \
         FROM agent_events WHERE session_id = ? ORDER BY created_at ASC",
    )
    .bind(&session_id)
    .fetch_all(pool)
    .await
    .expect("select agent_events");

    assert!(
        !recs.is_empty(),
        "should have at least one agent_events row after stream_chat"
    );

    // Expect context_trace_signal from the server-driven loop.
    let trace_signal = recs.iter().find(|r| {
        r.try_get::<String, _>("event_type").ok().as_deref() == Some("context_trace_signal")
    });
    assert!(
        trace_signal.is_some(),
        "should have a context_trace_signal event. Found event types: {:?}",
        recs.iter()
            .filter_map(|r| r.try_get::<String, _>("event_type").ok())
            .collect::<Vec<_>>()
    );

    let trace = trace_signal.unwrap();
    let event_id = trace.try_get::<String, _>("event_id").unwrap_or_default();
    assert!(
        !event_id.is_empty(),
        "context_trace_signal should have an event_id"
    );

    let chain_id = trace
        .try_get::<Option<String>, _>("causal_chain_id")
        .ok()
        .flatten()
        .unwrap_or_default();
    assert!(
        chain_id.contains("context-trace"),
        "causal_chain_id should contain 'context-trace', got: {chain_id}"
    );

    // ── Cleanup ──
    cleanup_session_data(pool, &session_id).await;
    ctx.pool.close().await;
}

/// B3: Multiple stream_chat calls to same session → multiple context trace events.
pub async fn run_stream_multi_turn_persistence() {
    let b = bootstrap().await;
    let ctx = &b.ctx;
    let app = &ctx.app;
    let auth = &b.auth_header;
    let pool = &ctx.pool;

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

    // Wait for Turn 1 events to persist.
    wait_for_agent_events_count(pool, &session_id, 1, std::time::Duration::from_secs(15)).await;

    let count_after_turn1: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM agent_events WHERE session_id = ?")
            .bind(&session_id)
            .fetch_one(pool)
            .await
            .unwrap_or(0);
    assert!(
        count_after_turn1 >= 1,
        "turn 1 should have >= 1 events, got {count_after_turn1}"
    );

    // ── Turn 2 ──
    let payload2 = json!({
        "message": "multi-turn message two",
        "session_id": &session_id,
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

    // Wait for Turn 2 events.
    wait_for_agent_events_count(
        pool,
        &session_id,
        count_after_turn1 + 1,
        std::time::Duration::from_secs(15),
    )
    .await;

    let count_after_turn2: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM agent_events WHERE session_id = ?")
            .bind(&session_id)
            .fetch_one(pool)
            .await
            .unwrap_or(0);
    assert!(
        count_after_turn2 > count_after_turn1,
        "turn 2 should add events: after_turn1={count_after_turn1}, after_turn2={count_after_turn2}"
    );

    // Verify multiple causal_chain_ids (each turn gets its own chain).
    let chains: Vec<String> = sqlx::query_scalar(
        "SELECT DISTINCT causal_chain_id FROM agent_events \
         WHERE session_id = ? AND causal_chain_id IS NOT NULL",
    )
    .bind(&session_id)
    .fetch_all(pool)
    .await
    .unwrap_or_default();
    assert!(
        chains.len() >= 2,
        "multi-turn should produce >= 2 distinct causal chains, got {} ({:?})",
        chains.len(),
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
    cleanup_session_data(pool, &session_id).await;
    ctx.pool.close().await;
}
