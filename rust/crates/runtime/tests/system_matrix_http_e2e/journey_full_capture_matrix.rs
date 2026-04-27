//! Session-scoped full LLM exchange capture with real MatrixOne-backed session metadata.

use astra_services::session_journal::{JournalDirGuard, JournalWriter};
use axum::http::StatusCode;
use axum::{body::Body, http::Request};
use futures_util::StreamExt;
use serde_json::{Value, json};
use tempfile::tempdir;
use tower::util::ServiceExt;

use super::harness::{bootstrap, cleanup_session_data, put_json};

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

fn read_journal_events(session_id: &str) -> Vec<Value> {
    let path = JournalWriter::new(session_id)
        .expect("journal writer")
        .path()
        .clone();
    let text =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read journal {path:?}: {e}"));
    text.lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).expect("valid journal line"))
        .collect()
}

async fn wait_for_full_capture_events(session_id: &str) -> Vec<Value> {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let events = read_journal_events(session_id);
        let llm_full: Vec<_> = events
            .iter()
            .filter(|event| {
                matches!(
                    event.get("type").and_then(Value::as_str),
                    Some("llm_request_full" | "llm_response_full")
                )
            })
            .cloned()
            .collect();
        if llm_full.len() >= 2 {
            return llm_full;
        }
        if tokio::time::Instant::now() >= deadline {
            panic!(
                "timeout waiting for llm_request_full/llm_response_full for session {session_id}"
            );
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}

pub async fn run_stream_session_metadata_enables_full_llm_exchange_journaling() {
    let Some(test_secret) = std::env::var("ASTRA_BRIDGE_TEST_SECRET").ok() else {
        // Bridge journal test requires ASTRA_BRIDGE_TEST_SECRET == CHAT_TURN_BRIDGE_SECRET.
        // Without it the bridge auth fails and no journal is written.
        // Mark as explicitly skipped rather than silently passing.
        panic!(
            "ASTRA_BRIDGE_TEST_SECRET not set — set it to the same value as CHAT_TURN_BRIDGE_SECRET to run this test"
        );
    };
    let temp = tempdir().expect("tempdir");
    let _guard = JournalDirGuard::new(temp.path());

    let b = bootstrap().await;
    let ctx = &b.ctx;
    let app = &ctx.app;
    let auth = &b.auth_header;
    let session_id = ctx.session_id.clone();

    let (st_put, put_j) = put_json(
        app,
        &format!("/sessions/{session_id}"),
        Some(auth),
        json!({
            "metadata": {
                "suite": "full_capture_matrix",
                "full_llm_capture": true
            }
        }),
    )
    .await;
    assert_eq!(st_put, StatusCode::OK, "update session metadata: {put_j}");
    assert_eq!(put_j["metadata"]["full_llm_capture"], true);

    let payload = json!({
        "message": "matrix full capture probe",
        "session_id": &session_id,
        "context": {
            "test_llm_stream_blocks": [
                "data: {\"type\":\"text_delta\",\"content\":\"Matrix capture verified.\"}\n\n",
                "data: {\"type\":\"_inprocess_summary\",\"full_text\":\"Matrix capture verified.\",\"reasoning\":\"\",\"tool_calls\":[],\"usage\":{\"prompt\":10,\"completion\":4,\"total\":14},\"model_used\":\"bridge-e2e-mock\"}\n\n"
            ]
        }
    });
    let req = Request::builder()
        .method("POST")
        .uri("/chat/stream")
        .header("authorization", auth)
        .header("content-type", "application/json")
        .header("x-mo-bridge-test-secret", &test_secret)
        .body(Body::from(payload.to_string()))
        .expect("stream request");
    let (status, body) = collect_full_sse_stream(app, req, 30).await;
    assert_eq!(status, StatusCode::OK, "chat/stream should return 200");
    assert!(body.contains("\"type\":\"session_info\""), "body: {body}");
    assert!(
        body.contains("Matrix capture verified."),
        "expected streamed assistant text in body: {body}"
    );

    let llm_events = wait_for_full_capture_events(&session_id).await;
    assert_eq!(
        llm_events[0]["type"].as_str(),
        Some("llm_request_full"),
        "first full-capture event should be request"
    );
    assert_eq!(
        llm_events[1]["type"].as_str(),
        Some("llm_response_full"),
        "second full-capture event should be response"
    );
    assert_eq!(
        llm_events[0]["metadata"]["request"]["messages"]
            .as_array()
            .and_then(|msgs| msgs.iter().find(|m| m["role"].as_str() == Some("user")))
            .and_then(|m| m["role"].as_str()),
        Some("user")
    );
    assert_eq!(
        llm_events[1]["metadata"]["response"]["response"]["full_text"].as_str(),
        Some("Matrix capture verified.")
    );

    cleanup_session_data(&ctx.pool, &session_id).await;
    ctx.pool.close().await;
}
