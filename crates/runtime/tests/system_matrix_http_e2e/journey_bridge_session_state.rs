//! CLI bridge session-state contract: real MatrixOne + full Axum wiring +
//! deterministic mock LLM. This is intentionally an online-gate journey, not
//! an in-memory bridge test, because the product failures here are caused by
//! disagreement between independently materialized database views.

use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use futures_util::StreamExt;
use serde_json::{Value, json};
use sqlx::Row;
use tower::util::ServiceExt;
use uuid::Uuid;

use super::harness::{
    bootstrap, cleanup_session_data, get_json, post_json, seeded_selected_model,
    sse_first_data_json_with_type, wait_for_agent_event_types,
};

const REAL_USER_MESSAGE: &str = "review the branch with three independent agents";
const FIRST_REPLY: &str = "Three reviews are running; I will wait for durable results.";
const RECONCILIATION_ENVELOPE: &str =
    astra_turn_core::chat_turn_edge_profile::RUNTIME_RECONCILIATION_USER_ENVELOPE;
const RECONCILED_REPLY: &str = "All three durable review results are now reconciled.";

async fn run_mock_bridge_turn(
    app: &axum::Router,
    auth: &str,
    session_id: &str,
    selected_model: Value,
    session_turn: u32,
    messages: Vec<Value>,
    edge_profile: Value,
    reply: &str,
    prompt_tokens: u64,
    completion_tokens: u64,
) -> (String, String) {
    let turn_chain_id = format!("bridge-state-chain-{}", Uuid::new_v4());
    let user_query_event_id = format!("bridge-state-query-{}", Uuid::new_v4());
    let payload = json!({
        "agent_id": "astra-cli",
        "session_id": session_id,
        "messages": messages,
        "selected_model": selected_model,
        "edge_tools": [],
        "edge_profile": edge_profile,
        "session_turn": session_turn,
        "turn_chain_id": turn_chain_id,
        "user_query_event_id": user_query_event_id,
        "test_llm_rounds": [{
            "full_text": reply,
            "reasoning": "",
            "usage": {
                "prompt_tokens": prompt_tokens,
                "completion_tokens": completion_tokens,
                "total_tokens": prompt_tokens + completion_tokens
            }
        }]
    });
    let test_secret = std::env::var("ASTRA_TEST_BRIDGE_SECRET").expect("bridge test secret");
    let request = Request::builder()
        .method("POST")
        .uri("/chat/turn")
        .header("authorization", auth)
        .header("content-type", "application/json")
        .header("x-mo-bridge-test-secret", test_secret)
        .body(Body::from(payload.to_string()))
        .expect("bridge state request");
    let response = app.clone().oneshot(request).await.expect("bridge oneshot");
    assert_eq!(response.status(), StatusCode::OK, "bridge turn status");

    let mut stream = response.into_body().into_data_stream();
    let mut bytes = Vec::new();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(20);
    while let Ok(Some(chunk)) = tokio::time::timeout_at(deadline, stream.next()).await {
        bytes.extend_from_slice(&chunk.expect("bridge SSE chunk"));
    }
    let sse = String::from_utf8_lossy(&bytes).into_owned();
    assert!(
        sse.contains("turn_complete"),
        "bridge turn did not complete: {sse}"
    );
    assert!(sse.contains(reply), "bridge reply missing from SSE: {sse}");
    let session_info = sse_first_data_json_with_type(&sse, "session_info")
        .unwrap_or_else(|| panic!("missing session_info: {sse}"));
    assert_eq!(session_info["session_id"].as_str(), Some(session_id));
    let run_id = session_info["run_id"]
        .as_str()
        .expect("session_info.run_id")
        .to_string();
    (sse, run_id)
}

/// A single journey locks the state contract at the boundaries users consume:
/// raw DB rows, session/events APIs, audit projections, and transcript.
pub async fn run_cli_bridge_session_views_remain_consistent() {
    let b = bootstrap().await;
    let ctx = &b.ctx;
    let app = &ctx.app;
    let auth = &b.auth_header;
    let pool = &ctx.pool;
    let user_id = &ctx.user_id;

    let (status, created) = post_json(
        app,
        "/sessions",
        Some(auth),
        json!({
            "title": "bridge session state contract",
            "metadata": {"suite": "bridge_session_state"}
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "create session: {created}");
    let session_id = created["session_id"]
        .as_str()
        .expect("created session_id")
        .to_string();

    let first_messages = vec![json!({"role": "user", "content": REAL_USER_MESSAGE})];
    let (_, first_run_id) = run_mock_bridge_turn(
        app,
        auth,
        &session_id,
        seeded_selected_model(ctx),
        1,
        first_messages.clone(),
        json!({}),
        FIRST_REPLY,
        11,
        5,
    )
    .await;

    let mut reconciliation_messages = first_messages;
    reconciliation_messages.push(json!({"role": "assistant", "content": FIRST_REPLY}));
    reconciliation_messages.push(json!({"role": "user", "content": RECONCILIATION_ENVELOPE}));
    let (_, reconciliation_run_id) = run_mock_bridge_turn(
        app,
        auth,
        &session_id,
        seeded_selected_model(ctx),
        2,
        reconciliation_messages,
        json!({
            astra_turn_core::chat_turn_edge_profile::EDGE_PROFILE_KEY_RUNTIME_RECONCILIATION_TURN: true,
            astra_turn_core::chat_turn_edge_profile::EDGE_PROFILE_KEY_RUNTIME_REQUIRED_TEXTS: [
                "Background agent results are terminal and ready for reconciliation."
            ]
        }),
        RECONCILED_REPLY,
        19,
        7,
    )
    .await;
    assert_ne!(first_run_id, reconciliation_run_id);

    wait_for_agent_event_types(
        pool,
        user_id,
        &session_id,
        &[
            "user_query",
            "runtime_reconciliation",
            "llm_response",
            "routing_decision",
            "context_trace_signal",
        ],
        std::time::Duration::from_secs(20),
    )
    .await;

    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let transcript_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM session_transcript_items WHERE user_id = ? AND session_id = ?",
        )
        .bind(user_id)
        .bind(&session_id)
        .fetch_one(pool)
        .await
        .expect("count bridge transcript rows");
        if transcript_count == 3 {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "expected exactly three bridge transcript rows, got {transcript_count}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }

    let core_rows = sqlx::query(
        "SELECT event_id, run_id, event_type, content, parent_event_id, turn_seq, token_input, token_output \
         FROM agent_events \
         WHERE user_id = ? AND session_id = ? \
           AND event_type IN ('user_query', 'runtime_reconciliation', 'llm_response') \
         ORDER BY turn_seq ASC, created_at ASC, event_id ASC",
    )
    .bind(user_id)
    .bind(&session_id)
    .fetch_all(pool)
    .await
    .expect("load bridge core rows");
    assert_eq!(core_rows.len(), 4, "core rows: {core_rows:?}");

    let user_rows = core_rows
        .iter()
        .filter(|row| row.get::<String, _>("event_type") == "user_query")
        .collect::<Vec<_>>();
    assert_eq!(user_rows.len(), 1, "only human input is a user turn");
    assert_eq!(user_rows[0].get::<String, _>("content"), REAL_USER_MESSAGE);

    let runtime_row = core_rows
        .iter()
        .find(|row| row.get::<String, _>("event_type") == "runtime_reconciliation")
        .expect("runtime reconciliation event");
    assert_eq!(
        runtime_row.get::<String, _>("content"),
        RECONCILIATION_ENVELOPE
    );
    assert_eq!(runtime_row.get::<i64, _>("turn_seq"), 2);

    let response_rows = core_rows
        .iter()
        .filter(|row| row.get::<String, _>("event_type") == "llm_response")
        .collect::<Vec<_>>();
    assert_eq!(response_rows.len(), 2);
    let persisted_run_ids = response_rows
        .iter()
        .map(|row| row.get::<String, _>("run_id"))
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(
        persisted_run_ids,
        std::collections::BTreeSet::from([first_run_id, reconciliation_run_id]),
        "SSE run identities and durable event run identities must agree"
    );
    assert_eq!(
        response_rows
            .iter()
            .map(|row| row.get::<i64, _>("token_input"))
            .sum::<i64>(),
        30
    );
    assert_eq!(
        response_rows
            .iter()
            .map(|row| row.get::<i64, _>("token_output"))
            .sum::<i64>(),
        12
    );

    let db_event_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM agent_events WHERE user_id = ? AND session_id = ?",
    )
    .bind(user_id)
    .bind(&session_id)
    .fetch_one(pool)
    .await
    .expect("count session events");
    let stored_event_count: i64 = sqlx::query_scalar(
        "SELECT event_count FROM agent_sessions WHERE user_id = ? AND session_id = ?",
    )
    .bind(user_id)
    .bind(&session_id)
    .fetch_one(pool)
    .await
    .expect("load session event_count");
    assert_eq!(stored_event_count, db_event_count);

    let (status, session) =
        get_json(app, &format!("/sessions/{session_id}"), Some(auth), &[]).await;
    assert_eq!(status, StatusCode::OK, "session view: {session}");
    assert_eq!(session["status"].as_str(), Some("active"));
    assert_eq!(session["event_count"].as_i64(), Some(db_event_count));

    let (status, events) = get_json(
        app,
        &format!("/events/session/{session_id}?limit=100"),
        Some(auth),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "events view: {events}");
    assert_eq!(events["total"].as_i64(), Some(db_event_count));
    assert_eq!(
        events["events"].as_array().map(Vec::len),
        Some(usize::try_from(db_event_count).expect("event count fits usize"))
    );
    assert_eq!(
        events["events"]
            .as_array()
            .expect("events array")
            .iter()
            .filter(|event| event["event_type"].as_str() == Some("user_query"))
            .count(),
        1
    );

    let (status, summary) = get_json(
        app,
        &format!("/sessions/{session_id}/audit/summary"),
        Some(auth),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "audit summary: {summary}");
    assert_eq!(summary["session_id"].as_str(), Some(session_id.as_str()));
    assert_eq!(summary["turn_count"].as_u64(), Some(1));
    assert_eq!(summary["tokens_in"].as_u64(), Some(30));
    assert_eq!(summary["tokens_out"].as_u64(), Some(12));
    assert!(
        summary["models_used"]
            .as_array()
            .is_some_and(|models| models.iter().any(|model| model == "bridge-e2e-mock")),
        "audit must expose the persisted mock model: {summary}"
    );

    let (status, turns) = get_json(
        app,
        &format!("/sessions/{session_id}/audit/turns?page=1&per_page=20"),
        Some(auth),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "audit turns: {turns}");
    assert_eq!(turns["total"].as_u64(), Some(1));
    assert_eq!(turns["turns"].as_array().map(Vec::len), Some(1));
    assert_eq!(
        turns["turns"][0]["user_input_preview"].as_str(),
        Some(REAL_USER_MESSAGE)
    );

    let (status, transcript) = get_json(
        app,
        &format!("/sessions/{session_id}/transcript?scope=root_conversation&limit=20"),
        Some(auth),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "transcript view: {transcript}");
    let items = transcript["items"].as_array().expect("transcript items");
    assert_eq!(items.len(), 3, "transcript: {transcript}");
    let roles_and_content = items
        .iter()
        .map(|item| {
            (
                item["role"].as_str().unwrap_or_default(),
                item["content"].as_str().unwrap_or_default(),
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        roles_and_content,
        vec![
            ("user", REAL_USER_MESSAGE),
            ("assistant", FIRST_REPLY),
            ("assistant", RECONCILED_REPLY),
        ]
    );
    assert!(
        items.iter().all(|item| item["run_id"].is_null()),
        "CLI-local run ids must not become orphan durable run references: {transcript}"
    );
    assert!(
        items
            .iter()
            .all(|item| item["content"].as_str() != Some(RECONCILIATION_ENVELOPE)),
        "runtime envelope must not appear as user speech: {transcript}"
    );

    cleanup_session_data(&ctx.shared_pool, user_id, &session_id).await;
    ctx.pool.close().await;
}
