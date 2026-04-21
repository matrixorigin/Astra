//! Journeys that do not belong in the monolithic product matrix: session cancel/delete, `/chat/stream`,
//! auth/session negative paths (replaces stub `auth_contract` / `session_contract` coverage),
//! models admin CRUD with DB checks (replaces `model_crud_contract`).
use axum::http::StatusCode;
use futures_util::StreamExt;
use serde_json::{Value, json};
use sqlx::Row;
use std::time::Duration;

use super::harness::{
    E2E_PASSWORD, E2eAuthMode, bootstrap, collect_sse_body_text, delete_no_content, get_json,
    grant_astra_admin_role, post_empty, post_json, post_json_with_headers, put_json,
    wait_for_agent_event_types,
};
use astra_services::session_journal::{JournalEventType, read_journal};
use axum::{body::Body, http::Request};
use tower::util::ServiceExt;

fn parse_sse_events(raw: &str) -> Vec<Value> {
    raw.lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .filter_map(|data| serde_json::from_str(data).ok())
        .collect()
}

pub async fn run_session_cancel_then_delete() {
    let b = bootstrap().await;
    let ctx = &b.ctx;
    let app = &ctx.app;
    let auth = &b.auth_header;
    let pool = &ctx.pool;
    let session_id = ctx.session_id.clone();

    let (st_can, can_j) = post_empty(
        app,
        &format!("/sessions/{session_id}/cancel"),
        Some(auth.as_str()),
    )
    .await;
    assert_eq!(st_can, StatusCode::OK, "cancel session: {can_j}");
    assert_eq!(
        can_j["status"].as_str(),
        Some("cancelled"),
        "cancel response: {can_j}"
    );

    let row = sqlx::query("SELECT status FROM agent_sessions WHERE session_id = ?")
        .bind(&session_id)
        .fetch_optional(pool)
        .await
        .expect("select session after cancel");
    let row = row.expect("session row after cancel");
    assert_eq!(
        row.try_get::<String, _>("status").ok().as_deref(),
        Some("cancelled")
    );

    let st_del =
        delete_no_content(app, &format!("/sessions/{session_id}"), Some(auth.as_str())).await;
    assert_eq!(st_del, StatusCode::NO_CONTENT, "delete session");

    let (st_get, _) = get_json(
        app,
        &format!("/sessions/{session_id}"),
        Some(auth.as_str()),
        &[],
    )
    .await;
    assert_eq!(st_get, StatusCode::NOT_FOUND, "get after delete");

    ctx.pool.close().await;
}

/// Unauthenticated `/sessions`, duplicate register, and bad password login (real DB + services).
/// Memory proxy must overwrite spoofed `user_id` / `session_id` with the authenticated user (real
/// `MemoriaForwarder` + JWT). Replaces stub `memory_contract` security coverage.
pub async fn run_memory_proxy_user_isolation() {
    let b = bootstrap().await;
    let ctx = &b.ctx;
    let app = &ctx.app;
    let auth = &b.auth_header;
    let user_id = ctx.user_id.as_str();

    let (st_unauth, j_unauth) = post_json(
        app,
        "/memory/store",
        None,
        json!({ "content": "x", "memory_type": "semantic" }),
    )
    .await;
    assert_eq!(
        st_unauth,
        StatusCode::UNAUTHORIZED,
        "memory without auth: {j_unauth}"
    );

    let before = ctx.memoria.calls.lock().await.len();
    let (st_spoof, j_spoof) = post_json(
        app,
        "/memory/store",
        Some(auth.as_str()),
        json!({
            "content": "spoof probe",
            "memory_type": "semantic",
            "user_id": "victim-user-id",
            "session_id": "victim-session-id"
        }),
    )
    .await;
    assert_eq!(st_spoof, StatusCode::OK, "memory store: {j_spoof}");

    let calls = ctx.memoria.calls.lock().await;
    assert!(
        calls.len() > before,
        "memoria forwarder should record /memory/store"
    );
    let (_, body) = calls.last().expect("last memoria call");
    assert_eq!(
        body["user_id"].as_str(),
        Some(user_id),
        "spoofed user_id must be replaced: {body}"
    );
    assert_eq!(
        body["session_id"].as_str(),
        Some(user_id),
        "spoofed session_id must be replaced with authenticated user_id: {body}"
    );

    ctx.pool.close().await;
}

pub async fn run_auth_and_session_negative_paths() {
    let b = bootstrap().await;
    let ctx = &b.ctx;
    let app = &ctx.app;

    let (st_sess, j_sess) = get_json(app, "/sessions", None, &[]).await;
    assert_eq!(
        st_sess,
        StatusCode::UNAUTHORIZED,
        "GET /sessions without auth: {j_sess}"
    );

    match b.auth_mode {
        E2eAuthMode::LocalJwt => {
            let dup_email = format!("dup_{}@e2e.test", ctx.suffix);
            let (st_dup, j_dup) = post_json(
                app,
                "/auth/register",
                None,
                json!({
                    "username": ctx.username,
                    "email": dup_email,
                    "password": "DifferentPass-1",
                    "display_name": "duplicate probe"
                }),
            )
            .await;
            assert_eq!(
                st_dup,
                StatusCode::BAD_REQUEST,
                "duplicate username register: {j_dup}"
            );
            assert_eq!(
                j_dup["detail"].as_str(),
                Some("Username already exists"),
                "duplicate username detail: {j_dup}"
            );

            let (st_bad_login, j_bad) = post_json(
                app,
                "/auth/login",
                None,
                json!({ "username": ctx.username, "password": "wrong-password-not-real" }),
            )
            .await;
            assert_eq!(
                st_bad_login,
                StatusCode::UNAUTHORIZED,
                "bad password login: {j_bad}"
            );
            assert_eq!(
                j_bad["detail"].as_str(),
                Some("Invalid username or password"),
                "bad login detail: {j_bad}"
            );

            // Sanity: login still works after negative calls.
            let (st_ok, j_ok) = post_json(
                app,
                "/auth/login",
                None,
                json!({ "username": ctx.username, "password": E2E_PASSWORD }),
            )
            .await;
            assert_eq!(st_ok, StatusCode::OK, "login still ok: {j_ok}");
        }
        E2eAuthMode::TrustedMoi => {
            for (path, payload) in [
                (
                    "/auth/register",
                    json!({
                        "username": "should-not-work",
                        "email": "should-not-work@e2e.test",
                        "password": "ignored"
                    }),
                ),
                ("/auth/login", json!({ "username": "x", "password": "y" })),
                ("/auth/refresh", json!({ "refresh_token": "not-used" })),
            ] {
                let (status, body) = post_json(app, path, None, payload).await;
                assert_eq!(
                    status,
                    StatusCode::FORBIDDEN,
                    "trusted_moi local auth endpoint should be disabled: {path} {body}"
                );
                assert_eq!(
                    body["detail"].as_str(),
                    Some("Local auth endpoints are disabled in trusted_moi mode"),
                    "trusted_moi local auth detail: {path} {body}"
                );
            }
        }
    }

    ctx.pool.close().await;
}

pub async fn run_chat_stream_session_info_smoke() {
    let b = bootstrap().await;
    let ctx = &b.ctx;
    let app = &ctx.app;
    let auth = &b.auth_header;
    let session_id = ctx.session_id.clone();

    // Test /chat/stream with mock LLM via bridge e2e hooks
    let test_secret = std::env::var("ASTRA_BRIDGE_TEST_SECRET").expect("bridge test secret");
    let body = json!({
        "message": "matrix e2e stream smoke",
        "session_id": session_id,
        "max_candidates": 1,
        "test_llm_rounds": [{
            "role": "assistant",
            "content": "stream smoke reply",
            "usage": { "prompt": 3, "completion": 5, "total": 8 }
        }]
    });
    let req = Request::builder()
        .method("POST")
        .uri("/chat/stream")
        .header("authorization", auth.as_str())
        .header("content-type", "application/json")
        .header("x-mo-bridge-test-secret", &test_secret)
        .body(Body::from(body.to_string()))
        .expect("stream request");
    let (st, text) = collect_sse_body_text(app, req, 512 * 1024).await;
    assert_eq!(
        st,
        StatusCode::OK,
        "chat/stream status, body prefix: {}",
        &text[..text.len().min(500)]
    );
    assert!(
        text.contains("data: "),
        "expected SSE data frames from chat/stream: {}",
        &text[..text.len().min(500)]
    );

    ctx.pool.close().await;
}

pub async fn run_chat_turn_unknown_session_not_found() {
    let b = bootstrap().await;
    let ctx = &b.ctx;
    let app = &ctx.app;
    let auth = &b.auth_header;
    let test_secret = std::env::var("ASTRA_BRIDGE_TEST_SECRET").expect("bridge test secret");
    let missing_session_id = format!("missing-session-{}", ctx.suffix);

    let (status, body) = post_json_with_headers(
        app,
        "/chat/turn",
        Some(auth.as_str()),
        &[("x-mo-bridge-test-secret", test_secret.as_str())],
        json!({
            "session_id": missing_session_id,
            "messages": [{ "role": "user", "content": "should fail before streaming" }],
            "test_llm_rounds": [{
                "full_text": "this mock round should never run"
            }]
        }),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "chat/turn unknown session: {body}"
    );
    assert_eq!(
        body["detail"].as_str(),
        Some("Session not found"),
        "chat/turn should normalize missing-session errors: {body}"
    );

    ctx.pool.close().await;
}

pub async fn run_approval_respond_invalid_session_id_rejected() {
    let b = bootstrap().await;
    let ctx = &b.ctx;
    let app = &ctx.app;
    let auth = &b.auth_header;

    let (status, body) = post_json(
        app,
        "/approval/respond",
        Some(auth.as_str()),
        json!({
            "request_id": format!("bad-session-{}", ctx.suffix),
            "decision": "allow",
            "session_id": "../escape"
        }),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "approval/respond should reject unsafe session ids: {body}"
    );
    assert!(
        body["detail"]
            .as_str()
            .is_some_and(|detail| detail.contains("invalid session ID")),
        "invalid session detail should explain the rejected session id: {body}"
    );

    ctx.pool.close().await;
}

pub async fn run_edge_callback_http_boundary_failures() {
    let b = bootstrap().await;
    let ctx = &b.ctx;
    let app = &ctx.app;
    let auth = &b.auth_header;

    let (st_tool_unauth, tool_unauth) = post_json(
        app,
        "/tools/result",
        None,
        json!({
            "request_id": format!("tool-unauth-{}", ctx.suffix),
            "status": "ok",
            "output": "ignored"
        }),
    )
    .await;
    assert_eq!(
        st_tool_unauth,
        StatusCode::UNAUTHORIZED,
        "/tools/result without auth should fail: {tool_unauth}"
    );

    let (st_appr_unauth, appr_unauth) = post_json(
        app,
        "/approval/respond",
        None,
        json!({
            "request_id": format!("approval-unauth-{}", ctx.suffix),
            "decision": "allow"
        }),
    )
    .await;
    assert_eq!(
        st_appr_unauth,
        StatusCode::UNAUTHORIZED,
        "/approval/respond without auth should fail: {appr_unauth}"
    );

    let (st_tool_bad, tool_bad) = post_json(
        app,
        "/tools/result",
        Some(auth.as_str()),
        json!({
            "status": "ok",
            "output": "missing request id"
        }),
    )
    .await;
    assert!(
        matches!(
            st_tool_bad,
            StatusCode::BAD_REQUEST | StatusCode::UNPROCESSABLE_ENTITY
        ),
        "/tools/result bad payload should be rejected: {st_tool_bad} {tool_bad}"
    );

    let (st_appr_bad, appr_bad) = post_json(
        app,
        "/approval/respond",
        Some(auth.as_str()),
        json!({
            "request_id": format!("approval-bad-{}", ctx.suffix),
            "decision": "not-a-real-decision"
        }),
    )
    .await;
    assert!(
        matches!(
            st_appr_bad,
            StatusCode::BAD_REQUEST | StatusCode::UNPROCESSABLE_ENTITY
        ),
        "/approval/respond bad payload should be rejected: {st_appr_bad} {appr_bad}"
    );

    ctx.pool.close().await;
}

pub async fn run_duplicate_tool_result_is_idempotent() {
    let b = bootstrap().await;
    let ctx = &b.ctx;
    let test_secret = std::env::var("ASTRA_BRIDGE_TEST_SECRET").expect("bridge test secret");
    let tool_output = "duplicate tool result ok";
    let payload = json!({
        "agent_id": "system-matrix-dup-tool-agent",
        "session_id": ctx.session_id,
        "messages": [{ "role": "user", "content": "read the duplicate path" }],
        "edge_tools": [{
            "type": "function",
            "function": {
                "name": "read_file",
                "description": "read a file",
                "parameters": {
                    "type": "object",
                    "properties": { "path": { "type": "string" } },
                    "required": ["path"]
                }
            }
        }],
        "test_llm_rounds": [
            {
                "tool_calls": [{
                    "id": "tc-dup-tool-1",
                    "type": "function",
                    "function": {
                        "name": "read_file",
                        "arguments": "{\"path\":\"dup.txt\"}"
                    }
                }]
            },
            {
                "full_text": "Done after duplicate tool result."
            }
        ]
    });

    let req = Request::builder()
        .method("POST")
        .uri("/chat/turn")
        .header("authorization", b.auth_header.as_str())
        .header("content-type", "application/json")
        .header("x-mo-bridge-test-secret", test_secret)
        .body(Body::from(payload.to_string()))
        .expect("duplicate tool result request");
    let response = ctx
        .app
        .clone()
        .oneshot(req)
        .await
        .expect("chat/turn oneshot");
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "chat/turn should return 200"
    );

    let mut stream = response.into_body().into_data_stream();
    let mut acc = Vec::new();
    let mut posted_duplicates = false;
    let mut saw_turn_complete = false;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.expect("duplicate tool result sse chunk");
        acc.extend_from_slice(&chunk);
        let s = String::from_utf8_lossy(&acc);
        if !posted_duplicates
            && s.contains("\"type\":\"tool_request\"")
            && s.contains("tc-dup-tool-1")
        {
            for _ in 0..2 {
                let (status, body) = post_json(
                    &ctx.app,
                    "/tools/result",
                    Some(b.auth_header.as_str()),
                    json!({
                        "request_id": "tc-dup-tool-1",
                        "status": "ok",
                        "output": tool_output,
                    }),
                )
                .await;
                assert_eq!(status, StatusCode::OK, "duplicate /tools/result: {body}");
            }
            posted_duplicates = true;
        }
        if s.contains("\"type\":\"turn_complete\"") {
            saw_turn_complete = true;
            break;
        }
    }
    assert!(posted_duplicates, "chat/turn never emitted tool_request");
    assert!(saw_turn_complete, "chat/turn never reached turn_complete");

    let full = String::from_utf8_lossy(&acc).into_owned();
    let events = parse_sse_events(&full);
    let tool_call_ends = events
        .iter()
        .filter(|event| {
            event.get("type").and_then(Value::as_str) == Some("tool_call_end")
                && event.get("call_id").and_then(Value::as_str) == Some("tc-dup-tool-1")
        })
        .count();
    assert_eq!(
        tool_call_ends, 1,
        "duplicate /tools/result should still yield exactly one tool_call_end: {events:?}"
    );
    assert!(
        full.contains("Done after duplicate tool result."),
        "second mock round should complete after duplicate callback: {full}"
    );

    wait_for_agent_event_types(
        &ctx.pool,
        &ctx.session_id,
        &["tool_result"],
        Duration::from_secs(30),
    )
    .await;
    let persisted: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM agent_events \
         WHERE session_id = ? AND event_type = 'tool_result' AND content LIKE ?",
    )
    .bind(&ctx.session_id)
    .bind(format!("%{tool_output}%"))
    .fetch_one(&ctx.pool)
    .await
    .expect("duplicate tool_result count");
    assert_eq!(
        persisted, 1,
        "duplicate /tools/result should persist one tool_result for the turn"
    );

    ctx.pool.close().await;
}

pub async fn run_duplicate_approval_response_is_idempotent() {
    let b = bootstrap().await;
    let ctx = &b.ctx;
    let test_secret = std::env::var("ASTRA_BRIDGE_TEST_SECRET").expect("bridge test secret");
    let tool_output = "duplicate approval ok";
    let payload = json!({
        "agent_id": "system-matrix-dup-approval-agent",
        "session_id": ctx.session_id,
        "messages": [{ "role": "user", "content": "write dup.txt" }],
        "edge_tools": [{
            "type": "function",
            "function": {
                "name": "write_file",
                "description": "write a file",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": { "type": "string" },
                        "content": { "type": "string" }
                    },
                    "required": ["path", "content"]
                }
            }
        }],
        "test_llm_rounds": [
            {
                "tool_calls": [{
                    "id": "tc-dup-appr-1",
                    "type": "function",
                    "function": {
                        "name": "write_file",
                        "arguments": "{\"path\":\"dup.txt\",\"content\":\"x\"}"
                    }
                }]
            },
            {
                "full_text": "Done after duplicate approval."
            }
        ]
    });

    let req = Request::builder()
        .method("POST")
        .uri("/chat/turn")
        .header("authorization", b.auth_header.as_str())
        .header("content-type", "application/json")
        .header("x-mo-bridge-test-secret", test_secret)
        .body(Body::from(payload.to_string()))
        .expect("duplicate approval request");
    let response = ctx
        .app
        .clone()
        .oneshot(req)
        .await
        .expect("chat/turn oneshot");
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "chat/turn should return 200"
    );

    let mut stream = response.into_body().into_data_stream();
    let mut acc = Vec::new();
    let mut posted_approval_duplicates = false;
    let mut posted_tool_result = false;
    let mut saw_turn_complete = false;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.expect("duplicate approval sse chunk");
        acc.extend_from_slice(&chunk);
        let s = String::from_utf8_lossy(&acc);
        if !posted_approval_duplicates
            && s.contains("\"type\":\"approval_required\"")
            && s.contains("tc-dup-appr-1")
        {
            for _ in 0..2 {
                let (status, body) = post_json(
                    &ctx.app,
                    "/approval/respond",
                    Some(b.auth_header.as_str()),
                    json!({
                        "request_id": "tc-dup-appr-1",
                        "decision": "allow",
                        "reason": "duplicate allow",
                        "session_id": ctx.session_id,
                        "tool_name": "write_file",
                        "approval_kind": "standard"
                    }),
                )
                .await;
                assert_eq!(
                    status,
                    StatusCode::OK,
                    "duplicate /approval/respond: {body}"
                );
            }
            posted_approval_duplicates = true;
        }
        if posted_approval_duplicates
            && !posted_tool_result
            && s.contains("\"type\":\"tool_request\"")
            && s.contains("tc-dup-appr-1")
        {
            let (status, body) = post_json(
                &ctx.app,
                "/tools/result",
                Some(b.auth_header.as_str()),
                json!({
                    "request_id": "tc-dup-appr-1",
                    "status": "ok",
                    "output": tool_output,
                }),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "tool result after approval: {body}");
            posted_tool_result = true;
        }
        if s.contains("\"type\":\"turn_complete\"") {
            saw_turn_complete = true;
            break;
        }
    }
    assert!(
        posted_approval_duplicates,
        "chat/turn never emitted approval_required"
    );
    assert!(
        posted_tool_result,
        "approved mutation never emitted tool_request"
    );
    assert!(saw_turn_complete, "chat/turn never reached turn_complete");

    let full = String::from_utf8_lossy(&acc).into_owned();
    let events = parse_sse_events(&full);
    let approvals = events
        .iter()
        .filter(|event| {
            event.get("type").and_then(Value::as_str) == Some("approval_required")
                && event.get("request_id").and_then(Value::as_str) == Some("tc-dup-appr-1")
        })
        .count();
    let tool_requests = events
        .iter()
        .filter(|event| {
            event.get("type").and_then(Value::as_str) == Some("tool_request")
                && event.get("request_id").and_then(Value::as_str) == Some("tc-dup-appr-1")
        })
        .count();
    let tool_call_ends = events
        .iter()
        .filter(|event| {
            event.get("type").and_then(Value::as_str) == Some("tool_call_end")
                && event.get("call_id").and_then(Value::as_str) == Some("tc-dup-appr-1")
        })
        .count();
    assert_eq!(
        approvals, 1,
        "duplicate approvals should not duplicate SSE approvals"
    );
    assert_eq!(
        tool_requests, 1,
        "duplicate approvals should still yield one tool_request: {events:?}"
    );
    assert_eq!(
        tool_call_ends, 1,
        "duplicate approvals should still yield one tool_call_end: {events:?}"
    );
    assert!(
        full.contains("Done after duplicate approval."),
        "second mock round should complete after duplicate approval: {full}"
    );

    let approval_decisions = read_journal(&ctx.session_id)
        .expect("read approval journal")
        .into_iter()
        .filter(|event| event.event_type == JournalEventType::ApprovalDecision)
        .filter(|event| {
            event
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.get("approval"))
                .and_then(|approval| approval.get("request_id"))
                .and_then(Value::as_str)
                == Some("tc-dup-appr-1")
        })
        .count();
    assert_eq!(
        approval_decisions, 1,
        "duplicate /approval/respond should record a single approval decision"
    );

    ctx.pool.close().await;
}

pub async fn run_chat_turn_partial_batch_failure() {
    let b = bootstrap().await;
    let ctx = &b.ctx;
    let test_secret = std::env::var("ASTRA_BRIDGE_TEST_SECRET").expect("bridge test secret");
    let ok_output = "partial batch first ok";
    let err_output = "partial batch second failed";
    let payload = json!({
        "agent_id": "system-matrix-partial-batch-agent",
        "session_id": ctx.session_id,
        "messages": [{ "role": "user", "content": "read two files and continue even if one fails" }],
        "edge_tools": [{
            "type": "function",
            "function": {
                "name": "read_file",
                "description": "read a file",
                "parameters": {
                    "type": "object",
                    "properties": { "path": { "type": "string" } },
                    "required": ["path"]
                }
            }
        }],
        "test_llm_rounds": [
            {
                "tool_calls": [
                    {
                        "id": "tc-partial-1",
                        "type": "function",
                        "function": {
                            "name": "read_file",
                            "arguments": "{\"path\":\"a.txt\"}"
                        }
                    },
                    {
                        "id": "tc-partial-2",
                        "type": "function",
                        "function": {
                            "name": "read_file",
                            "arguments": "{\"path\":\"b.txt\"}"
                        }
                    }
                ]
            },
            {
                "full_text": "Handled the partial batch failure."
            }
        ]
    });

    let req = Request::builder()
        .method("POST")
        .uri("/chat/turn")
        .header("authorization", b.auth_header.as_str())
        .header("content-type", "application/json")
        .header("x-mo-bridge-test-secret", test_secret)
        .body(Body::from(payload.to_string()))
        .expect("partial batch request");
    let response = ctx
        .app
        .clone()
        .oneshot(req)
        .await
        .expect("chat/turn oneshot");
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "chat/turn should return 200"
    );

    let mut stream = response.into_body().into_data_stream();
    let mut acc = Vec::new();
    let mut posted_first = false;
    let mut posted_second = false;
    let mut saw_turn_complete = false;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.expect("partial batch sse chunk");
        acc.extend_from_slice(&chunk);
        let s = String::from_utf8_lossy(&acc);
        if !posted_first && s.contains("\"type\":\"tool_request\"") && s.contains("tc-partial-1") {
            let (status, body) = post_json(
                &ctx.app,
                "/tools/result",
                Some(b.auth_header.as_str()),
                json!({
                    "request_id": "tc-partial-1",
                    "status": "ok",
                    "output": ok_output,
                }),
            )
            .await;
            assert_eq!(
                status,
                StatusCode::OK,
                "first partial /tools/result: {body}"
            );
            posted_first = true;
        }
        if !posted_second && s.contains("\"type\":\"tool_request\"") && s.contains("tc-partial-2") {
            let (status, body) = post_json(
                &ctx.app,
                "/tools/result",
                Some(b.auth_header.as_str()),
                json!({
                    "request_id": "tc-partial-2",
                    "status": "error",
                    "output": err_output,
                }),
            )
            .await;
            assert_eq!(
                status,
                StatusCode::OK,
                "second partial /tools/result: {body}"
            );
            posted_second = true;
        }
        if s.contains("\"type\":\"turn_complete\"") {
            saw_turn_complete = true;
            break;
        }
    }
    assert!(
        posted_first,
        "partial batch never emitted first tool_request"
    );
    assert!(
        posted_second,
        "partial batch never emitted second tool_request"
    );
    assert!(
        saw_turn_complete,
        "partial batch never reached turn_complete"
    );

    let full = String::from_utf8_lossy(&acc).into_owned();
    let events = parse_sse_events(&full);
    let tool_requests = events
        .iter()
        .filter(|event| event.get("type").and_then(Value::as_str) == Some("tool_request"))
        .count();
    assert_eq!(
        tool_requests, 2,
        "expected two tool_request events: {events:?}"
    );

    let first_end = events.iter().find(|event| {
        event.get("type").and_then(Value::as_str) == Some("tool_call_end")
            && event.get("call_id").and_then(Value::as_str) == Some("tc-partial-1")
    });
    let second_end = events.iter().find(|event| {
        event.get("type").and_then(Value::as_str) == Some("tool_call_end")
            && event.get("call_id").and_then(Value::as_str) == Some("tc-partial-2")
    });
    let first_end = first_end.expect("first tool_call_end");
    let second_end = second_end.expect("second tool_call_end");
    assert!(
        first_end
            .get("result")
            .map(Value::to_string)
            .unwrap_or_default()
            .contains(ok_output),
        "first tool_call_end should carry success output: {first_end}"
    );
    assert!(
        second_end
            .get("result")
            .map(Value::to_string)
            .unwrap_or_default()
            .contains(err_output),
        "second tool_call_end should carry failure output: {second_end}"
    );
    assert!(
        full.contains("Handled the partial batch failure."),
        "second mock round should complete after mixed results: {full}"
    );

    wait_for_agent_event_types(
        &ctx.pool,
        &ctx.session_id,
        &["tool_result"],
        Duration::from_secs(30),
    )
    .await;
    let persisted: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM agent_events \
         WHERE session_id = ? AND event_type = 'tool_result' AND (content LIKE ? OR content LIKE ?)",
    )
    .bind(&ctx.session_id)
    .bind(format!("%{ok_output}%"))
    .bind(format!("%{err_output}%"))
    .fetch_one(&ctx.pool)
    .await
    .expect("partial batch tool_result count");
    assert_eq!(
        persisted, 2,
        "expected both mixed tool results to persist exactly once"
    );

    ctx.pool.close().await;
}

pub async fn run_chat_turn_out_of_order_tool_results() {
    let b = bootstrap().await;
    let ctx = &b.ctx;
    let test_secret = std::env::var("ASTRA_BRIDGE_TEST_SECRET").expect("bridge test secret");
    let first_output = "race first ok";
    let second_output = "race second ok";
    let payload = json!({
        "agent_id": "system-matrix-race-agent",
        "session_id": ctx.session_id,
        "messages": [{ "role": "user", "content": "read two files even if callbacks arrive out of order" }],
        "edge_tools": [{
            "type": "function",
            "function": {
                "name": "read_file",
                "description": "read a file",
                "parameters": {
                    "type": "object",
                    "properties": { "path": { "type": "string" } },
                    "required": ["path"]
                }
            }
        }],
        "test_llm_rounds": [
            {
                "tool_calls": [
                    {
                        "id": "tc-race-1",
                        "type": "function",
                        "function": {
                            "name": "read_file",
                            "arguments": "{\"path\":\"race-a.txt\"}"
                        }
                    },
                    {
                        "id": "tc-race-2",
                        "type": "function",
                        "function": {
                            "name": "read_file",
                            "arguments": "{\"path\":\"race-b.txt\"}"
                        }
                    }
                ]
            },
            {
                "full_text": "Handled out-of-order callback delivery."
            }
        ]
    });

    let req = Request::builder()
        .method("POST")
        .uri("/chat/turn")
        .header("authorization", b.auth_header.as_str())
        .header("content-type", "application/json")
        .header("x-mo-bridge-test-secret", test_secret)
        .body(Body::from(payload.to_string()))
        .expect("out-of-order request");
    let response = ctx
        .app
        .clone()
        .oneshot(req)
        .await
        .expect("chat/turn oneshot");
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "chat/turn should return 200"
    );

    let mut stream = response.into_body().into_data_stream();
    let mut acc = Vec::new();
    let mut posted_out_of_order = false;
    let mut saw_turn_complete = false;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.expect("out-of-order sse chunk");
        acc.extend_from_slice(&chunk);
        let s = String::from_utf8_lossy(&acc);
        if !posted_out_of_order
            && s.contains("\"type\":\"tool_request\"")
            && s.contains("tc-race-1")
            && s.contains("tc-race-2")
        {
            let (second, first) = tokio::join!(
                post_json(
                    &ctx.app,
                    "/tools/result",
                    Some(b.auth_header.as_str()),
                    json!({
                        "request_id": "tc-race-2",
                        "status": "ok",
                        "output": second_output,
                    }),
                ),
                async {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                    post_json(
                        &ctx.app,
                        "/tools/result",
                        Some(b.auth_header.as_str()),
                        json!({
                            "request_id": "tc-race-1",
                            "status": "ok",
                            "output": first_output,
                        }),
                    )
                    .await
                }
            );
            assert_eq!(
                second.0,
                StatusCode::OK,
                "second out-of-order result: {}",
                second.1
            );
            assert_eq!(
                first.0,
                StatusCode::OK,
                "first out-of-order result: {}",
                first.1
            );
            posted_out_of_order = true;
        }
        if s.contains("\"type\":\"turn_complete\"") {
            saw_turn_complete = true;
            break;
        }
    }
    assert!(
        posted_out_of_order,
        "chat/turn never emitted both tool_request callbacks"
    );
    assert!(
        saw_turn_complete,
        "chat/turn never completed after out-of-order callbacks"
    );

    let full = String::from_utf8_lossy(&acc).into_owned();
    let events = parse_sse_events(&full);
    let tool_requests = events
        .iter()
        .filter(|event| event.get("type").and_then(Value::as_str) == Some("tool_request"))
        .count();
    assert_eq!(
        tool_requests, 2,
        "expected two tool_request events: {events:?}"
    );

    let first_end = events.iter().find(|event| {
        event.get("type").and_then(Value::as_str) == Some("tool_call_end")
            && event.get("call_id").and_then(Value::as_str) == Some("tc-race-1")
    });
    let second_end = events.iter().find(|event| {
        event.get("type").and_then(Value::as_str) == Some("tool_call_end")
            && event.get("call_id").and_then(Value::as_str) == Some("tc-race-2")
    });
    let first_end = first_end.expect("first out-of-order tool_call_end");
    let second_end = second_end.expect("second out-of-order tool_call_end");
    assert!(
        first_end
            .get("result")
            .map(Value::to_string)
            .unwrap_or_default()
            .contains(first_output),
        "first tool_call_end should carry the first output: {first_end}"
    );
    assert!(
        second_end
            .get("result")
            .map(Value::to_string)
            .unwrap_or_default()
            .contains(second_output),
        "second tool_call_end should carry the second output: {second_end}"
    );
    assert!(
        full.contains("Handled out-of-order callback delivery."),
        "second mock round should complete after out-of-order callbacks: {full}"
    );

    wait_for_agent_event_types(
        &ctx.pool,
        &ctx.session_id,
        &["tool_result"],
        Duration::from_secs(30),
    )
    .await;
    let persisted: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM agent_events \
         WHERE session_id = ? AND event_type = 'tool_result' AND (content LIKE ? OR content LIKE ?)",
    )
    .bind(&ctx.session_id)
    .bind(format!("%{first_output}%"))
    .bind(format!("%{second_output}%"))
    .fetch_one(&ctx.pool)
    .await
    .expect("out-of-order tool_result count");
    assert_eq!(
        persisted, 2,
        "expected both out-of-order tool results to persist exactly once"
    );

    ctx.pool.close().await;
}

pub async fn run_same_session_concurrent_turns_isolated() {
    let b = bootstrap().await;
    let ctx = &b.ctx;
    let test_secret = std::env::var("ASTRA_BRIDGE_TEST_SECRET").expect("bridge test secret");
    let session_id = ctx.session_id.clone();
    let payload_a = json!({
        "agent_id": "system-matrix-overlap-agent",
        "session_id": session_id,
        "messages": [{ "role": "user", "content": "same-session overlap request A" }],
        "test_llm_rounds": [{
            "full_text": "Overlap response A"
        }]
    });
    let payload_b = json!({
        "agent_id": "system-matrix-overlap-agent",
        "session_id": ctx.session_id,
        "messages": [{ "role": "user", "content": "same-session overlap request B" }],
        "test_llm_rounds": [{
            "full_text": "Overlap response B"
        }]
    });

    let collect_turn = |app: axum::Router, auth: String, payload: Value, secret: String| async move {
        let req = Request::builder()
            .method("POST")
            .uri("/chat/turn")
            .header("authorization", auth)
            .header("content-type", "application/json")
            .header("x-mo-bridge-test-secret", secret)
            .body(Body::from(payload.to_string()))
            .expect("overlap request");
        let response = app.clone().oneshot(req).await.expect("chat/turn oneshot");
        assert_eq!(
            response.status(),
            StatusCode::OK,
            "chat/turn should return 200"
        );

        let mut stream = response.into_body().into_data_stream();
        let mut acc = Vec::new();
        let mut saw_turn_complete = false;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.expect("overlap sse chunk");
            acc.extend_from_slice(&chunk);
            if String::from_utf8_lossy(&acc).contains("\"type\":\"turn_complete\"") {
                saw_turn_complete = true;
                break;
            }
        }
        assert!(
            saw_turn_complete,
            "overlap turn never reached turn_complete"
        );
        String::from_utf8_lossy(&acc).into_owned()
    };

    let (raw_a, raw_b) = tokio::join!(
        collect_turn(
            ctx.app.clone(),
            b.auth_header.clone(),
            payload_a,
            test_secret.clone(),
        ),
        collect_turn(
            ctx.app.clone(),
            b.auth_header.clone(),
            payload_b,
            test_secret
        ),
    );

    assert!(
        raw_a.contains("Overlap response A"),
        "first concurrent turn should keep its own response: {raw_a}"
    );
    assert!(
        raw_b.contains("Overlap response B"),
        "second concurrent turn should keep its own response: {raw_b}"
    );

    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let llm_rows: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM agent_events \
             WHERE session_id = ? AND event_type = 'llm_response' AND (content = ? OR content = ?)",
        )
        .bind(&ctx.session_id)
        .bind("Overlap response A")
        .bind("Overlap response B")
        .fetch_one(&ctx.pool)
        .await
        .expect("overlap llm_response count");
        if llm_rows >= 2 {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for concurrent same-session llm_response rows"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    let distinct_event_ids: i64 = sqlx::query_scalar(
        "SELECT COUNT(DISTINCT event_id) FROM agent_events \
         WHERE session_id = ? AND event_type = 'llm_response' AND (content = ? OR content = ?)",
    )
    .bind(&ctx.session_id)
    .bind("Overlap response A")
    .bind("Overlap response B")
    .fetch_one(&ctx.pool)
    .await
    .expect("overlap distinct event_id count");
    assert_eq!(
        distinct_event_ids, 2,
        "same-session overlap should persist distinct llm_response event IDs"
    );

    let distinct_chain_ids: i64 = sqlx::query_scalar(
        "SELECT COUNT(DISTINCT causal_chain_id) FROM agent_events \
         WHERE session_id = ? AND event_type = 'llm_response' AND (content = ? OR content = ?)",
    )
    .bind(&ctx.session_id)
    .bind("Overlap response A")
    .bind("Overlap response B")
    .fetch_one(&ctx.pool)
    .await
    .expect("overlap distinct causal_chain_id count");
    assert_eq!(
        distinct_chain_ids, 2,
        "same-session overlap should preserve distinct causal chains"
    );

    ctx.pool.close().await;
}

pub async fn run_same_session_waiting_turn_overlap_isolated() {
    let b = bootstrap().await;
    let ctx = &b.ctx;
    let test_secret = std::env::var("ASTRA_BRIDGE_TEST_SECRET").expect("bridge test secret");
    let tool_output = "waiting overlap tool ok";
    let tool_turn_payload = json!({
        "agent_id": "system-matrix-overlap-agent",
        "session_id": ctx.session_id,
        "messages": [{ "role": "user", "content": "waiting overlap tool turn" }],
        "edge_tools": [{
            "type": "function",
            "function": {
                "name": "read_file",
                "description": "read a file",
                "parameters": {
                    "type": "object",
                    "properties": { "path": { "type": "string" } },
                    "required": ["path"]
                }
            }
        }],
        "test_llm_rounds": [
            {
                "tool_calls": [{
                    "id": "tc-overlap-wait-1",
                    "type": "function",
                    "function": {
                        "name": "read_file",
                        "arguments": "{\"path\":\"wait.txt\"}"
                    }
                }]
            },
            {
                "full_text": "Waiting overlap tool turn finished."
            }
        ]
    });

    let collect_turn = |app: axum::Router, auth: String, payload: Value, secret: String| async move {
        let req = Request::builder()
            .method("POST")
            .uri("/chat/turn")
            .header("authorization", auth)
            .header("content-type", "application/json")
            .header("x-mo-bridge-test-secret", secret)
            .body(Body::from(payload.to_string()))
            .expect("waiting-overlap request");
        let response = app.clone().oneshot(req).await.expect("chat/turn oneshot");
        assert_eq!(
            response.status(),
            StatusCode::OK,
            "chat/turn should return 200"
        );

        let mut stream = response.into_body().into_data_stream();
        let mut acc = Vec::new();
        let mut saw_turn_complete = false;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.expect("waiting-overlap sse chunk");
            acc.extend_from_slice(&chunk);
            if String::from_utf8_lossy(&acc).contains("\"type\":\"turn_complete\"") {
                saw_turn_complete = true;
                break;
            }
        }
        assert!(
            saw_turn_complete,
            "waiting-overlap turn never reached turn_complete"
        );
        String::from_utf8_lossy(&acc).into_owned()
    };

    let req = Request::builder()
        .method("POST")
        .uri("/chat/turn")
        .header("authorization", b.auth_header.as_str())
        .header("content-type", "application/json")
        .header("x-mo-bridge-test-secret", &test_secret)
        .body(Body::from(tool_turn_payload.to_string()))
        .expect("waiting overlap tool request");
    let response = ctx
        .app
        .clone()
        .oneshot(req)
        .await
        .expect("chat/turn oneshot");
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "chat/turn should return 200"
    );

    let mut stream = response.into_body().into_data_stream();
    let mut acc = Vec::new();
    let mut ran_overlap_turn = false;
    let mut overlap_raw = String::new();
    let mut posted_tool_result = false;
    let mut saw_turn_complete = false;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.expect("waiting overlap primary sse chunk");
        acc.extend_from_slice(&chunk);
        let s = String::from_utf8_lossy(&acc);
        if !ran_overlap_turn
            && s.contains("\"type\":\"tool_request\"")
            && s.contains("tc-overlap-wait-1")
        {
            overlap_raw = collect_turn(
                ctx.app.clone(),
                b.auth_header.clone(),
                json!({
                    "agent_id": "system-matrix-overlap-agent",
                    "session_id": ctx.session_id,
                    "messages": [{ "role": "user", "content": "waiting overlap plain turn" }],
                    "test_llm_rounds": [{
                        "full_text": "Waiting overlap plain turn finished."
                    }]
                }),
                test_secret.clone(),
            )
            .await;
            ran_overlap_turn = true;

            let (status, body) = post_json(
                &ctx.app,
                "/tools/result",
                Some(b.auth_header.as_str()),
                json!({
                    "request_id": "tc-overlap-wait-1",
                    "status": "ok",
                    "output": tool_output,
                }),
            )
            .await;
            assert_eq!(
                status,
                StatusCode::OK,
                "waiting overlap tool result: {body}"
            );
            posted_tool_result = true;
        }
        if s.contains("\"type\":\"turn_complete\"") {
            saw_turn_complete = true;
            break;
        }
    }
    assert!(
        ran_overlap_turn,
        "primary tool-backed turn never emitted tool_request for overlap"
    );
    assert!(
        posted_tool_result,
        "waiting overlap never posted tool result"
    );
    assert!(
        saw_turn_complete,
        "tool-backed waiting overlap turn never reached turn_complete"
    );

    let primary_raw = String::from_utf8_lossy(&acc).into_owned();
    assert!(
        primary_raw.contains("Waiting overlap tool turn finished."),
        "tool-backed turn should keep its own final text: {primary_raw}"
    );
    assert!(
        overlap_raw.contains("Waiting overlap plain turn finished."),
        "overlap turn should keep its own final text: {overlap_raw}"
    );

    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let llm_rows: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM agent_events \
             WHERE session_id = ? AND event_type = 'llm_response' AND (content = ? OR content = ?)",
        )
        .bind(&ctx.session_id)
        .bind("Waiting overlap tool turn finished.")
        .bind("Waiting overlap plain turn finished.")
        .fetch_one(&ctx.pool)
        .await
        .expect("waiting overlap llm_response count");
        if llm_rows >= 2 {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for waiting-overlap llm_response rows"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    let distinct_event_ids: i64 = sqlx::query_scalar(
        "SELECT COUNT(DISTINCT event_id) FROM agent_events \
         WHERE session_id = ? AND event_type = 'llm_response' AND (content = ? OR content = ?)",
    )
    .bind(&ctx.session_id)
    .bind("Waiting overlap tool turn finished.")
    .bind("Waiting overlap plain turn finished.")
    .fetch_one(&ctx.pool)
    .await
    .expect("waiting overlap distinct event_id count");
    assert_eq!(
        distinct_event_ids, 2,
        "waiting overlap should persist distinct llm_response event IDs"
    );

    let distinct_chain_ids: i64 = sqlx::query_scalar(
        "SELECT COUNT(DISTINCT causal_chain_id) FROM agent_events \
         WHERE session_id = ? AND event_type = 'llm_response' AND (content = ? OR content = ?)",
    )
    .bind(&ctx.session_id)
    .bind("Waiting overlap tool turn finished.")
    .bind("Waiting overlap plain turn finished.")
    .fetch_one(&ctx.pool)
    .await
    .expect("waiting overlap distinct causal_chain_id count");
    assert_eq!(
        distinct_chain_ids, 2,
        "waiting overlap should preserve distinct causal chains"
    );

    ctx.pool.close().await;
}

/// Admin model CRUD with `infra_llm_models` assertions. Uses `provider: mock` so connectivity check
/// skips the network (`validate_connectivity` short-circuit).
pub async fn run_models_admin_crud_with_db() {
    let b = bootstrap().await;
    let ctx = &b.ctx;
    let app = &ctx.app;
    let auth = b.auth_header.as_str();
    let pool = &ctx.pool;
    let model_name = format!("e2e_mtx_mdl_{}", ctx.suffix);

    if b.auth_mode == E2eAuthMode::TrustedMoi {
        let (st_forbidden, body) = post_json(
            app,
            "/models",
            Some(auth),
            json!({
                "name": model_name,
                "provider": "mock",
                "api_key": "e2e-key-not-used"
            }),
        )
        .await;
        assert!(
            st_forbidden == StatusCode::UNAUTHORIZED || st_forbidden == StatusCode::FORBIDDEN,
            "trusted_moi admin model CRUD should be blocked by current admin auth path: {body}"
        );
        ctx.pool.close().await;
        return;
    }

    grant_astra_admin_role(&ctx.pool, &ctx.user_id).await;

    let (st_c, j_c) = post_json(
        app,
        "/models",
        Some(auth),
        json!({
            "name": model_name,
            "provider": "mock",
            "api_key": "e2e-key-not-used",
            "input_modalities": ["text"],
            "output_modalities": ["text"],
            "supported_parameters": ["temperature"],
            "tags": ["e2e_matrix"]
        }),
    )
    .await;
    assert_eq!(st_c, StatusCode::CREATED, "create model: {j_c}");
    assert_eq!(j_c["name"].as_str(), Some(model_name.as_str()));

    let row = sqlx::query("SELECT model_name, provider FROM infra_llm_models WHERE model_name = ?")
        .bind(&model_name)
        .fetch_optional(pool)
        .await
        .expect("select infra_llm_models");
    let row = row.expect("model row after create");
    assert_eq!(
        row.try_get::<String, _>("model_name").ok().as_deref(),
        Some(model_name.as_str())
    );
    assert_eq!(
        row.try_get::<String, _>("provider").ok().as_deref(),
        Some("mock")
    );

    let (st_u, j_u) = put_json(
        app,
        &format!("/models/{model_name}"),
        Some(auth),
        json!({ "description": "e2e matrix updated", "is_active": true }),
    )
    .await;
    assert_eq!(st_u, StatusCode::OK, "update model: {j_u}");
    assert_eq!(j_u["description"].as_str(), Some("e2e matrix updated"));

    let desc_row = sqlx::query("SELECT description FROM infra_llm_models WHERE model_name = ?")
        .bind(&model_name)
        .fetch_one(pool)
        .await
        .expect("description after update");
    assert_eq!(
        desc_row
            .try_get::<Option<String>, _>("description")
            .ok()
            .flatten()
            .as_deref(),
        Some("e2e matrix updated")
    );

    let st_d = delete_no_content(app, &format!("/models/{model_name}"), Some(auth)).await;
    assert_eq!(st_d, StatusCode::NO_CONTENT, "delete model");

    let gone = sqlx::query("SELECT 1 FROM infra_llm_models WHERE model_name = ?")
        .bind(&model_name)
        .fetch_optional(pool)
        .await
        .expect("select after delete");
    assert!(gone.is_none(), "model row should be deleted");

    let (st_g, _) = get_json(app, &format!("/models/{model_name}"), Some(auth), &[]).await;
    assert_eq!(st_g, StatusCode::NOT_FOUND, "get after delete");

    ctx.pool.close().await;
}
