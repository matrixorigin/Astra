//! Journeys that do not belong in the monolithic product matrix: session cancel/delete, `/chat/stream`,
//! auth/session negative paths (replaces stub `auth_contract` / `session_contract` coverage),
//! models admin CRUD with DB checks (replaces `model_crud_contract`).
use axum::http::StatusCode;
use futures_util::StreamExt;
use serde_json::{Value, json};
use sqlx::Row;
use std::time::Duration;

use super::harness::{
    E2E_PASSWORD, bootstrap, collect_sse_body_text, delete_json, delete_no_content,
    durable_interaction_event_count, get_json, grant_astra_admin_role,
    maybe_tool_result_payload_from_sse, post_empty, post_json, put_json, seed_pending_approval,
    seeded_model_selection, tool_result_payload,
};
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

    let row = sqlx::query("SELECT status FROM agent_sessions WHERE session_id = ? AND user_id = ?")
        .bind(&session_id)
        .bind(&ctx.user_id)
        .fetch_optional(pool)
        .await
        .expect("select session after cancel");
    let row = row.expect("session row after cancel");
    assert_eq!(
        row.try_get::<String, _>("status").ok().as_deref(),
        Some("cancelled")
    );

    let (st_del, del_j) =
        delete_json(app, &format!("/sessions/{session_id}"), Some(auth.as_str())).await;
    assert_eq!(st_del, StatusCode::NO_CONTENT, "delete session: {del_j}");

    let (st_get, _) = get_json(
        app,
        &format!("/sessions/{session_id}"),
        Some(auth.as_str()),
        &[],
    )
    .await;
    assert_eq!(st_get, StatusCode::NOT_FOUND, "get after delete");

    ctx.close().await;
}

/// Unauthenticated `/sessions`, duplicate register, and bad password login (real DB + services).
/// Memory proxy must overwrite spoofed `user_id` and reject a session not owned by that user.
/// An authorized durable session id remains distinct from the authenticated user id.
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
    assert_eq!(
        st_spoof,
        StatusCode::NOT_FOUND,
        "unowned memory session must be rejected: {j_spoof}"
    );
    assert_eq!(
        ctx.memoria.calls.lock().await.len(),
        before,
        "rejected session scope must not reach Memoria"
    );

    let (st_owned, j_owned) = post_json(
        app,
        "/memory/store",
        Some(auth.as_str()),
        json!({
            "content": "owned scope probe",
            "memory_type": "semantic",
            "user_id": "victim-user-id",
            "session_id": ctx.session_id
        }),
    )
    .await;
    assert_eq!(st_owned, StatusCode::OK, "owned memory store: {j_owned}");

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
        Some(ctx.session_id.as_str()),
        "authorized durable session_id must be preserved: {body}"
    );

    ctx.close().await;
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

    let (st_ok, j_ok) = post_json(
        app,
        "/auth/login",
        None,
        json!({ "username": ctx.username, "password": E2E_PASSWORD }),
    )
    .await;
    assert_eq!(st_ok, StatusCode::OK, "login still ok: {j_ok}");

    ctx.close().await;
}

pub async fn run_chat_stream_session_info_smoke() {
    let b = bootstrap().await;
    let ctx = &b.ctx;
    let app = &ctx.app;
    let auth = &b.auth_header;
    let session_id = ctx.session_id.clone();

    // The one admission is handled by the ServerAgenticLoopHost; mock rounds
    // are request context for the legacy test-only inference hook.
    let body = json!({
        "message": "matrix e2e stream smoke",
        "session_id": session_id,
        "model_selection": seeded_model_selection(ctx),
        "execution_budget": {
            "initial_turns": 1,
            "hard_turn_limit": 1
        },
        "context": {
            "test_llm_rounds": [{
                "full_text": "stream smoke reply",
                "usage": { "prompt_tokens": 3, "completion_tokens": 5, "total_tokens": 8 }
            }]
        }
    });
    let req = Request::builder()
        .method("POST")
        .uri("/chat/stream")
        .header("authorization", auth.as_str())
        .header("content-type", "application/json")
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

    ctx.close().await;
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
            "session_id": "../escape",
            "run_id": format!("bad-session-run-{}", ctx.suffix)
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

    ctx.close().await;
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
        tool_result_payload(astra_thin_client::ToolResultRequestParts {
            session_id: ctx.session_id.clone(),
            run_id: format!("run-tool-unauth-{}", ctx.suffix),
            turn_chain_id: format!("chain-tool-unauth-{}", ctx.suffix),
            request_id: format!("tool-unauth-{}", ctx.suffix),
            edge_agent_id: ctx.edge_agent_id.clone(),
            status: "completed".to_string(),
            output: "ignored".to_string(),
            duration_ms: 0,
            tool_result_fields: None,
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
            "decision": "allow",
            "session_id": ctx.session_id,
            "run_id": format!("approval-unauth-run-{}", ctx.suffix)
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
            "status": "completed",
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

    ctx.close().await;
}

pub async fn run_duplicate_tool_result_server_stream_is_idempotent() {
    let b = bootstrap().await;
    let ctx = &b.ctx;
    let tool_output = "duplicate tool result ok";
    let payload = json!({
        "agent_id": "system-matrix-dup-tool-agent",
        "session_id": ctx.session_id,
        "edge_executor_id": ctx.edge_agent_id,
        "workspace_binding": {
            "kind": "edge_workspace",
            "display_name": "system-matrix-edge",
            "root": "/tmp/astra-system-matrix-edge",
            "authority": "read_write"
        },
        "executor_binding": {
            "kind": "edge_agent",
            "executor_id": ctx.edge_agent_id,
            "display_name": "system-matrix-edge",
            "transport": "edge_ledger",
            "status": "online"
        },
        "message": "read the duplicate path",
        "model_selection": seeded_model_selection(ctx),
        "context": {
            "edge_profile": {
                "cwd": "/tmp/astra-system-matrix-edge",
                "edge_agent_id": ctx.edge_agent_id,
                "hostname": "system-matrix-edge"
            },
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
        }
    });

    let req = Request::builder()
        .method("POST")
        .uri("/chat/stream")
        .header("authorization", b.auth_header.as_str())
        .header("content-type", "application/json")
        .body(Body::from(payload.to_string()))
        .expect("duplicate tool result stream request");
    let response = ctx
        .app
        .clone()
        .oneshot(req)
        .await
        .expect("chat/stream oneshot");
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "chat/stream should return 200"
    );

    let mut stream = response.into_body().into_data_stream();
    let mut acc = Vec::new();
    let mut posted_duplicates = false;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.expect("duplicate tool result sse chunk");
        acc.extend_from_slice(&chunk);
        let s = String::from_utf8_lossy(&acc);
        if !posted_duplicates
            && let Some(payload) = maybe_tool_result_payload_from_sse(
                s.as_ref(),
                "tc-dup-tool-1",
                &ctx.edge_agent_id,
                "completed",
                tool_output,
                0,
            )
        {
            for _ in 0..2 {
                let (status, body) = post_json(
                    &ctx.app,
                    "/tools/result",
                    Some(b.auth_header.as_str()),
                    payload.clone(),
                )
                .await;
                assert_eq!(status, StatusCode::OK, "duplicate /tools/result: {body}");
            }
            posted_duplicates = true;
        }
    }
    assert!(posted_duplicates, "chat/stream never emitted tool_request");

    let full = String::from_utf8_lossy(&acc).into_owned();
    let events = parse_sse_events(&full);
    let tool_requests = events
        .iter()
        .filter(|event| {
            event.get("type").and_then(Value::as_str) == Some("tool_request")
                && event.get("request_id").and_then(Value::as_str) == Some("tc-dup-tool-1")
        })
        .count();
    assert_eq!(
        tool_requests, 1,
        "duplicate /tools/result should still yield exactly one tool_request: {events:?}"
    );
    let terminals = events
        .iter()
        .filter(|event| event.get("type").and_then(Value::as_str) == Some("turn_complete"))
        .collect::<Vec<_>>();
    assert_eq!(terminals.len(), 1, "one server stream terminal: {events:?}");
    let turn_complete = terminals[0];
    assert_eq!(
        turn_complete
            .get("continuation_owner")
            .and_then(Value::as_str),
        Some("server"),
        "duplicate callback must not turn the tool boundary into a terminal: {events:?}"
    );
    assert_eq!(
        turn_complete
            .get("tool_calls_count")
            .and_then(Value::as_u64),
        Some(1),
        "duplicate callback terminal must account for one tool call: {events:?}"
    );
    assert_eq!(
        turn_complete
            .get("tools_used")
            .and_then(Value::as_array)
            .map(|tools| tools.iter().filter_map(Value::as_str).collect::<Vec<_>>()),
        Some(vec!["read_file"]),
        "duplicate callback terminal must report the normalized tool list: {events:?}"
    );
    assert_eq!(
        turn_complete.get("llm_rounds").and_then(Value::as_u64),
        Some(2),
        "duplicate callback terminal must include the initial tool round and final model round: {events:?}"
    );
    assert!(
        events.iter().any(|event| {
            event.get("type").and_then(Value::as_str) == Some("text_delta")
                && event.get("content").and_then(Value::as_str)
                    == Some("Done after duplicate tool result.")
        }),
        "same stream must consume the duplicate callback and emit its final model round: {events:?}"
    );
    let run_id = events
        .iter()
        .find(|event| event["type"].as_str() == Some("session_info"))
        .and_then(|event| event["run_id"].as_str())
        .expect("session_info run_id")
        .to_string();
    let durable_results: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM agent_run_events \
         WHERE user_id = ? AND run_id = ? AND event_type = 'tool_call_end' \
           AND JSON_UNQUOTE(JSON_EXTRACT(payload_json, '$.call_id')) = ?",
    )
    .bind(&ctx.user_id)
    .bind(&run_id)
    .bind("tc-dup-tool-1")
    .fetch_one(&ctx.pool)
    .await
    .expect("count durable duplicate tool results");
    assert_eq!(
        durable_results, 1,
        "duplicate callback must have one durable effect"
    );

    ctx.close().await;
}

pub async fn run_duplicate_approval_response_is_idempotent() {
    let b = bootstrap().await;
    let ctx = &b.ctx;
    let run_id = format!("run-dup-appr-{}", ctx.suffix);
    seed_pending_approval(ctx, &run_id, "tc-dup-appr-1", "write_file", "standard").await;
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
                "run_id": run_id,
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

    let approval_decisions =
        durable_interaction_event_count(ctx, &run_id, "tc-dup-appr-1", "approval_resolved").await;
    assert_eq!(
        approval_decisions, 1,
        "duplicate /approval/respond should commit one durable terminal decision"
    );

    ctx.close().await;
}

pub async fn run_server_stream_partial_batch_failure() {
    let b = bootstrap().await;
    let ctx = &b.ctx;
    let ok_output = "partial batch first ok";
    let err_output = "partial batch second failed";
    let payload = json!({
        "agent_id": "system-matrix-partial-batch-agent",
        "session_id": ctx.session_id,
        "edge_executor_id": ctx.edge_agent_id,
        "workspace_binding": {
            "kind": "edge_workspace",
            "display_name": "system-matrix-edge",
            "root": "/tmp/astra-system-matrix-edge",
            "authority": "read_write"
        },
        "executor_binding": {
            "kind": "edge_agent",
            "executor_id": ctx.edge_agent_id,
            "display_name": "system-matrix-edge",
            "transport": "edge_ledger",
            "status": "online"
        },
        "message": "read two files and continue even if one fails",
        "model_selection": seeded_model_selection(ctx),
        "context": {
            "edge_profile": {
                "cwd": "/tmp/astra-system-matrix-edge",
                "edge_agent_id": ctx.edge_agent_id,
                "hostname": "system-matrix-edge"
            },
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
        }
    });

    let req = Request::builder()
        .method("POST")
        .uri("/chat/stream")
        .header("authorization", b.auth_header.as_str())
        .header("content-type", "application/json")
        .body(Body::from(payload.to_string()))
        .expect("partial batch stream request");
    let response = ctx
        .app
        .clone()
        .oneshot(req)
        .await
        .expect("chat/stream oneshot");
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "chat/stream should return 200"
    );

    let mut stream = response.into_body().into_data_stream();
    let mut acc = Vec::new();
    let mut posted_first = false;
    let mut posted_second = false;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.expect("partial batch sse chunk");
        acc.extend_from_slice(&chunk);
        let s = String::from_utf8_lossy(&acc);
        if !posted_first
            && let Some(payload) = maybe_tool_result_payload_from_sse(
                s.as_ref(),
                "tc-partial-1",
                &ctx.edge_agent_id,
                "completed",
                ok_output,
                0,
            )
        {
            let (status, body) = post_json(
                &ctx.app,
                "/tools/result",
                Some(b.auth_header.as_str()),
                payload,
            )
            .await;
            assert_eq!(
                status,
                StatusCode::OK,
                "first partial /tools/result: {body}"
            );
            posted_first = true;
        }
        if !posted_second
            && let Some(payload) = maybe_tool_result_payload_from_sse(
                s.as_ref(),
                "tc-partial-2",
                &ctx.edge_agent_id,
                // Edge callback statuses are a closed wire contract. Use the
                // canonical terminal failure rather than the old display-only
                // alias so this journey exercises a real failed receipt.
                "failed",
                err_output,
                0,
            )
        {
            let (status, body) = post_json(
                &ctx.app,
                "/tools/result",
                Some(b.auth_header.as_str()),
                payload,
            )
            .await;
            assert_eq!(
                status,
                StatusCode::OK,
                "second partial /tools/result: {body}"
            );
            posted_second = true;
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
    for request_id in ["tc-partial-1", "tc-partial-2"] {
        let ends = events
            .iter()
            .filter(|event| {
                event.get("type").and_then(Value::as_str) == Some("tool_call_end")
                    && event.get("call_id").and_then(Value::as_str) == Some(request_id)
            })
            .collect::<Vec<_>>();
        assert_eq!(
            ends.len(),
            1,
            "callback {request_id} must have exactly one raw SSE terminal: {events:?}"
        );
        assert!(
            ends[0]
                .as_object()
                .is_some_and(|event| event.keys().all(|key| !key.starts_with("_astra_"))),
            "callback {request_id} leaked an internal settlement field: {:?}",
            ends[0]
        );
    }

    let terminals = events
        .iter()
        .filter(|event| event.get("type").and_then(Value::as_str) == Some("turn_complete"))
        .collect::<Vec<_>>();
    assert_eq!(terminals.len(), 1, "one server stream terminal: {events:?}");
    let turn_complete = terminals[0];
    assert_eq!(
        turn_complete
            .get("continuation_owner")
            .and_then(Value::as_str),
        Some("server"),
        "mixed callback boundary must not be terminal before final model round: {events:?}"
    );
    assert_eq!(
        turn_complete
            .get("tool_calls_count")
            .and_then(Value::as_u64),
        Some(2),
        "mixed callback terminal must account for both tool calls: {events:?}"
    );
    assert_eq!(
        turn_complete
            .get("tools_used")
            .and_then(Value::as_array)
            .map(|tools| tools.iter().filter_map(Value::as_str).collect::<Vec<_>>()),
        Some(vec!["read_file"]),
        "mixed callback terminal must report a unique normalized tool list: {events:?}"
    );
    assert_eq!(
        turn_complete.get("llm_rounds").and_then(Value::as_u64),
        Some(2),
        "mixed callback terminal must include the initial tool round and final model round: {events:?}"
    );
    assert!(
        events.iter().any(|event| {
            event["type"].as_str() == Some("text_delta")
                && event["content"].as_str() == Some("Handled the partial batch failure.")
        }),
        "server stream must consume mixed callback results and emit final model text: {events:?}"
    );
    let run_id = events
        .iter()
        .find(|event| event["type"].as_str() == Some("session_info"))
        .and_then(|event| event["run_id"].as_str())
        .expect("session_info run_id")
        .to_string();
    for request_id in ["tc-partial-1", "tc-partial-2"] {
        let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM agent_run_events \
             WHERE user_id = ? AND run_id = ? AND event_type = 'tool_call_end' \
               AND JSON_UNQUOTE(JSON_EXTRACT(payload_json, '$.call_id')) = ?",
        )
        .bind(&ctx.user_id)
        .bind(&run_id)
        .bind(request_id)
        .fetch_one(&ctx.pool)
        .await
        .expect("count durable mixed callback result");
        assert_eq!(
            count, 1,
            "callback {request_id} should have one durable effect"
        );
    }

    ctx.close().await;
}

pub async fn run_server_stream_out_of_order_tool_results() {
    let b = bootstrap().await;
    let ctx = &b.ctx;
    let first_output = "race first ok";
    let second_output = "race second ok";
    let payload = json!({
        "agent_id": "system-matrix-race-agent",
        "session_id": ctx.session_id,
        "edge_executor_id": ctx.edge_agent_id,
        "workspace_binding": {
            "kind": "edge_workspace",
            "display_name": "system-matrix-edge",
            "root": "/tmp/astra-system-matrix-edge",
            "authority": "read_write"
        },
        "executor_binding": {
            "kind": "edge_agent",
            "executor_id": ctx.edge_agent_id,
            "display_name": "system-matrix-edge",
            "transport": "edge_ledger",
            "status": "online"
        },
        "message": "read two files even if callbacks arrive out of order",
        "model_selection": seeded_model_selection(ctx),
        "context": {
            "edge_profile": {
                "cwd": "/tmp/astra-system-matrix-edge",
                "edge_agent_id": ctx.edge_agent_id,
                "hostname": "system-matrix-edge"
            },
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
        }
    });

    let req = Request::builder()
        .method("POST")
        .uri("/chat/stream")
        .header("authorization", b.auth_header.as_str())
        .header("content-type", "application/json")
        .body(Body::from(payload.to_string()))
        .expect("out-of-order stream request");
    let response = ctx
        .app
        .clone()
        .oneshot(req)
        .await
        .expect("chat/stream oneshot");
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "chat/stream should return 200"
    );

    let mut stream = response.into_body().into_data_stream();
    let mut acc = Vec::new();
    let mut posted_out_of_order = false;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.expect("out-of-order sse chunk");
        acc.extend_from_slice(&chunk);
        let s = String::from_utf8_lossy(&acc);
        if !posted_out_of_order {
            let second_payload = maybe_tool_result_payload_from_sse(
                s.as_ref(),
                "tc-race-2",
                &ctx.edge_agent_id,
                "completed",
                second_output,
                0,
            );
            let first_payload = maybe_tool_result_payload_from_sse(
                s.as_ref(),
                "tc-race-1",
                &ctx.edge_agent_id,
                "completed",
                first_output,
                0,
            );
            let (Some(second_payload), Some(first_payload)) = (second_payload, first_payload)
            else {
                continue;
            };
            let (second, first) = tokio::join!(
                post_json(
                    &ctx.app,
                    "/tools/result",
                    Some(b.auth_header.as_str()),
                    second_payload,
                ),
                async {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                    post_json(
                        &ctx.app,
                        "/tools/result",
                        Some(b.auth_header.as_str()),
                        first_payload,
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
    }
    assert!(
        posted_out_of_order,
        "chat/stream never emitted both tool_request callbacks"
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
    for request_id in ["tc-race-1", "tc-race-2"] {
        let ends = events
            .iter()
            .filter(|event| {
                event.get("type").and_then(Value::as_str) == Some("tool_call_end")
                    && event.get("call_id").and_then(Value::as_str) == Some(request_id)
            })
            .collect::<Vec<_>>();
        assert_eq!(
            ends.len(),
            1,
            "callback {request_id} must have exactly one raw SSE terminal: {events:?}"
        );
        assert!(
            ends[0]
                .as_object()
                .is_some_and(|event| event.keys().all(|key| !key.starts_with("_astra_"))),
            "callback {request_id} leaked an internal settlement field: {:?}",
            ends[0]
        );
    }

    let terminals = events
        .iter()
        .filter(|event| event.get("type").and_then(Value::as_str) == Some("turn_complete"))
        .collect::<Vec<_>>();
    assert_eq!(terminals.len(), 1, "one server stream terminal: {events:?}");
    let turn_complete = terminals[0];
    assert_eq!(
        turn_complete
            .get("continuation_owner")
            .and_then(Value::as_str),
        Some("server"),
        "out-of-order callback boundary must not be terminal before final model round: {events:?}"
    );
    assert_eq!(
        turn_complete
            .get("tool_calls_count")
            .and_then(Value::as_u64),
        Some(2),
        "out-of-order callback terminal must account for both tool calls: {events:?}"
    );
    assert_eq!(
        turn_complete
            .get("tools_used")
            .and_then(Value::as_array)
            .map(|tools| tools.iter().filter_map(Value::as_str).collect::<Vec<_>>()),
        Some(vec!["read_file"]),
        "out-of-order callback terminal must report the normalized tool list: {events:?}"
    );
    assert_eq!(
        turn_complete.get("llm_rounds").and_then(Value::as_u64),
        Some(2),
        "out-of-order callback terminal must include the initial tool round and final model round: {events:?}"
    );
    assert!(
        events.iter().any(|event| {
            event["type"].as_str() == Some("text_delta")
                && event["content"].as_str() == Some("Handled out-of-order callback delivery.")
        }),
        "server stream must consume callbacks in identity order and emit final model text: {events:?}"
    );
    let run_id = events
        .iter()
        .find(|event| event["type"].as_str() == Some("session_info"))
        .and_then(|event| event["run_id"].as_str())
        .expect("session_info run_id")
        .to_string();
    for request_id in ["tc-race-1", "tc-race-2"] {
        let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM agent_run_events \
             WHERE user_id = ? AND run_id = ? AND event_type = 'tool_call_end' \
               AND JSON_UNQUOTE(JSON_EXTRACT(payload_json, '$.call_id')) = ?",
        )
        .bind(&ctx.user_id)
        .bind(&run_id)
        .bind(request_id)
        .fetch_one(&ctx.pool)
        .await
        .expect("count durable out-of-order callback result");
        assert_eq!(
            count, 1,
            "callback {request_id} should have one durable effect"
        );
    }

    ctx.close().await;
}

pub async fn run_models_admin_crud_with_db() {
    let b = bootstrap().await;
    let ctx = &b.ctx;
    let app = &ctx.app;
    let auth = b.auth_header.as_str();
    let pool = &ctx.pool;
    let model_name = format!("e2e_mtx_mdl_{}", ctx.suffix);

    grant_astra_admin_role(&ctx.pool, &ctx.user_id).await;

    let (st_c, j_c) = post_json(
        app,
        "/models",
        Some(auth),
        json!({
            "name": model_name,
            "provider": "mock",
            "context_window": 200000,
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

    ctx.close().await;
}
