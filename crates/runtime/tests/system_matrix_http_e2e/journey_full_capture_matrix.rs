//! Session-scoped full LLM exchange capture with real MatrixOne-backed session metadata.

use astra_services::session_journal::{JournalWriter, ProcessJournalDirGuard};
use axum::http::StatusCode;
use axum::{body::Body, http::Request};
use futures_util::StreamExt;
use serde_json::{Value, json};
use tempfile::tempdir;
use tower::util::ServiceExt;

use super::harness::{bootstrap, cleanup_session_data, put_json, seeded_model_selection};

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
    loop {
        match tokio::time::timeout_at(deadline, stream.next()).await {
            Ok(Some(chunk)) => {
                let chunk = chunk.expect("body chunk");
                acc.extend_from_slice(&chunk);
            }
            Ok(None) => break,
            Err(_) => panic!(
                "SSE stream did not terminate within {timeout_secs}s; collected {} bytes",
                acc.len()
            ),
        }
    }
    (status, String::from_utf8_lossy(&acc).into_owned())
}

fn read_journal_events(user_id: &str, session_id: &str) -> Vec<Value> {
    let path = JournalWriter::for_user(user_id, session_id)
        .expect("journal writer")
        .path()
        .clone();
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
        Err(error) => panic!("read journal {path:?}: {error}"),
    };
    text.lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).expect("valid journal line"))
        .collect()
}

fn is_full_capture_event(event: &Value) -> bool {
    matches!(
        event.get("type").and_then(Value::as_str),
        Some("llm_request_full" | "llm_response_full")
    )
}

fn is_request_event(event: &Value) -> bool {
    event.get("type").and_then(Value::as_str) == Some("llm_request_full")
}

fn is_response_event(event: &Value) -> bool {
    if event.get("type").and_then(Value::as_str) != Some("llm_response_full") {
        return false;
    }
    event["metadata"]["response"]["response"].is_object()
}

async fn wait_for_full_capture_events(user_id: &str, session_id: &str) -> Vec<Value> {
    // A preceding stream journey can still be draining its final durable
    // projection on this two-worker runtime. Keep the assertion strict, but
    // give the asynchronous journal hand-off the same bounded budget used by
    // other Matrix-backed durability checks.
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(15);
    loop {
        let events = read_journal_events(user_id, session_id);
        let llm_full: Vec<_> = events
            .iter()
            .filter(|event| is_full_capture_event(event))
            .cloned()
            .collect();
        let has_request = llm_full.iter().any(is_request_event);
        let has_response = llm_full.iter().any(is_response_event);
        if has_request && has_response {
            return llm_full;
        }
        if tokio::time::Instant::now() >= deadline {
            panic!(
                "timeout waiting for llm_request_full and llm_response_full \
                 for session {session_id}: {}",
                serde_json::to_string_pretty(&llm_full).unwrap()
            );
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}

pub async fn run_stream_session_metadata_enables_full_llm_exchange_journaling() {
    let Some(test_secret) = std::env::var("ASTRA_TEST_E2E_SECRET").ok() else {
        panic!("ASTRA_TEST_E2E_SECRET not set — deterministic inference is fail-closed");
    };
    let temp = tempdir().expect("tempdir");
    let _guard = ProcessJournalDirGuard::new(temp.path());

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
        "model_selection": seeded_model_selection(ctx),
        "context": {
            "test_llm_stream_blocks": [
                "data: {\"type\":\"text_delta\",\"content\":\"Matrix capture verified.\"}\n\n",
                "data: {\"type\":\"_inprocess_summary\",\"full_text\":\"Matrix capture verified.\",\"reasoning\":\"\",\"tool_calls\":[],\"usage\":{\"prompt\":10,\"completion\":4,\"total\":14},\"model_used\":\"server-e2e-mock\"}\n\n"
            ]
        }
    });
    let req = Request::builder()
        .method("POST")
        .uri("/chat/stream")
        .header("authorization", auth)
        .header("content-type", "application/json")
        .header("x-astra-e2e-test-secret", &test_secret)
        .body(Body::from(payload.to_string()))
        .expect("stream request");
    let (status, body) = collect_full_sse_stream(app, req, 30).await;
    assert_eq!(status, StatusCode::OK, "chat/stream should return 200");
    assert!(body.contains("\"type\":\"session_info\""), "body: {body}");
    assert!(
        body.contains("Matrix capture verified."),
        "expected streamed assistant text in body: {body}"
    );

    let llm_events = wait_for_full_capture_events(&ctx.user_id, &session_id).await;
    let request_event = llm_events
        .iter()
        .find(|event| is_request_event(event))
        .expect("full-capture request event");
    let response_event = llm_events
        .iter()
        .find(|event| is_response_event(event))
        .expect("full-capture response event");
    assert_eq!(
        request_event["type"].as_str(),
        Some("llm_request_full"),
        "full-capture event should include a request"
    );
    assert_eq!(
        response_event["type"].as_str(),
        Some("llm_response_full"),
        "full-capture event should include a structured response payload"
    );
    assert_eq!(
        request_event["metadata"]["request"]["messages"]
            .as_array()
            .and_then(|msgs| msgs.iter().find(|m| m["role"].as_str() == Some("user")))
            .and_then(|m| m["role"].as_str()),
        Some("user")
    );
    assert!(
        response_event["metadata"]["response"]["outcome"].is_string(),
        "response full-capture event: {}",
        serde_json::to_string_pretty(response_event).unwrap()
    );

    cleanup_session_data(&ctx.shared_pool, &ctx.user_id, &session_id).await;
    ctx.close().await;
}
