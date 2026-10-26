//! Journeys that do not belong in the monolithic product matrix: session cancel/delete, `/chat/stream`,
//! auth/session negative paths (replaces stub `auth_contract` / `session_contract` coverage),
//! models admin CRUD with DB checks (replaces `model_crud_contract`).
use super::harness::call_id_for;
use super::harness::{ProviderResponse, ProviderScript};
use axum::http::StatusCode;
use futures_util::StreamExt;
use serde_json::{Value, json};
use sqlx::Row;

use super::harness::{
    E2E_PASSWORD, MATRIX_E2E_EDGE_WORKSPACE_ROOT, bootstrap, delete_json, delete_no_content,
    durable_interaction_event_count, get_json, grant_astra_admin_role,
    maybe_tool_result_payload_from_sse, parse_sse_events, post_empty, post_json, put_json,
    seed_pending_approval, seeded_model_selection, tool_result_payload,
};
use axum::{body::Body, http::Request};
use tower::util::ServiceExt;

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

    // Exercise admission and completion through the actual provider request.
    let fixture_model = format!("mock-{}", ctx.suffix);
    ctx.install_native_provider(&b.auth_header,vec![ProviderScript::new("run_chat_stream_session_info_smoke",move |request| request.path=="/v1/chat/completions" && request.body["model"]==fixture_model && request.body["stream"]==true && request.body["messages"].as_array().is_some_and(|messages| messages.iter().any(|message|message["role"]=="user" && message["content"]=="matrix e2e stream smoke")),vec![ProviderResponse::OpenAi(json!({"choices":[{"index":0,"message":{"role":"assistant","content":"stream smoke reply","reasoning_content":"","tool_calls":[]},"finish_reason":"stop"}],"usage":{ "prompt_tokens": 3, "completion_tokens": 5, "total_tokens": 8 }}))])]).await;
    let body = json!({
        "message": "matrix e2e stream smoke",
    "execution_policy":{"turn_intent":"fixed_default","skill_auto_route":"disabled"},
        "session_id": session_id,
        "model_selection": seeded_model_selection(ctx),
        "execution_budget": {
            "initial_turns": 1,
            "hard_turn_limit": 1
        },
        "context": {

        }
    });
    let req = Request::builder()
        .method("POST")
        .uri("/chat/stream")
        .header("authorization", auth.as_str())
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .expect("stream request");
    let (st, text) = super::journey_stream_persistence::collect_full_sse_stream(app, req, 30).await;
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

    let events = parse_sse_events(&text);
    assert!(events.iter().any(|event| event["type"] == "session_info"));
    assert!(events.iter().any(|event| event["type"] == "turn_complete"));
    assert!(
        events
            .iter()
            .any(|event| event["type"] == "text_delta" && event["content"] == "stream smoke reply")
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

/// Verify actual model consumption, without persisting repeatable reads as effects.
async fn assert_readonly_callback_consumption(
    ctx: &super::harness::MatrixE2eCtx,
    events: &[Value],
    prompt: &str,
    expected: &[(&str, Value, &str, bool)],
) {
    let requests = ctx.native_provider_requests().await;
    let primary: Vec<_> = requests
        .iter()
        .filter(|request| {
            request.path == "/v1/chat/completions"
                && request.body["stream"] == true
                && request.body["model"] == format!("mock-{}", ctx.suffix)
                && request.body["messages"].as_array().is_some_and(|messages| {
                    messages
                        .iter()
                        .any(|message| message["role"] == "user" && message["content"] == prompt)
                })
        })
        .collect();
    assert_eq!(primary.len(), 2, "one tool round and one final model round");
    let messages = primary[1].body["messages"].as_array().unwrap();
    for (id, arguments, output, failed) in expected {
        let calls: Vec<_> = messages
            .iter()
            .filter_map(|message| message["tool_calls"].as_array())
            .flatten()
            .filter(|call| call["id"] == *id)
            .collect();
        assert_eq!(calls.len(), 1, "one original provider call {id}");
        assert_eq!(calls[0]["function"]["name"], "read_file");
        assert_eq!(
            serde_json::from_str::<Value>(calls[0]["function"]["arguments"].as_str().unwrap())
                .unwrap(),
            *arguments
        );
        let results: Vec<_> = messages
            .iter()
            .filter(|message| message["role"] == "tool" && message["tool_call_id"] == *id)
            .collect();
        assert_eq!(results.len(), 1, "one acknowledged result for {id}");
        let content = results[0]["content"].as_str().expect("model tool result");
        if *failed {
            let (result, guidance) = content
                .split_once("\n\n[Runtime tool guidance]\n")
                .expect("failed result keeps typed status and runtime advisory");
            assert_eq!(
                result,
                format!("status=failed\n{output}"),
                "failed result identity/payload {id}"
            );
            assert!(!guidance.is_empty());
        } else {
            assert_eq!(
                content, *output,
                "callback identity/output mismatch for {id}"
            );
        }
    }
    assert_eq!(
        events
            .iter()
            .filter(|event| event["type"] == "tool_request")
            .count(),
        expected.len()
    );
}

pub async fn run_duplicate_tool_result_server_stream_is_idempotent() {
    let b = bootstrap().await;
    let ctx = &b.ctx;
    let tool_output = "duplicate tool result ok";
    let fixture_model = format!("mock-{}", ctx.suffix);
    ctx.install_native_provider(&b.auth_header,vec![ProviderScript::new("run_duplicate_tool_result_server_stream_is_idempotent",move |request| request.path=="/v1/chat/completions" && request.body["model"]==fixture_model && request.body["stream"]==true && request.body["messages"].as_array().is_some_and(|messages| messages.iter().any(|message|message["role"]=="user" && message["content"]=="read the duplicate path")),vec![ProviderResponse::OpenAi(json!({"choices":[{"index":0,"message":{"role":"assistant","content":"","reasoning_content":"","tool_calls":[{
                        "id": "tc-dup-tool-1",
                        "type": "function",
                        "function": {
                            "name": "read_file",
                            "arguments": "{\"path\":\"dup.txt\"}"
                        }
                    }]},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":42,"completion_tokens":7,"total_tokens":49}})),ProviderResponse::OpenAi(json!({"choices":[{"index":0,"message":{"role":"assistant","content":"Done after duplicate tool result.","reasoning_content":"","tool_calls":[]},"finish_reason":"stop"}],"usage":{"prompt_tokens":42,"completion_tokens":7,"total_tokens":49}}))])]).await;
    let payload = json!({
        "agent_id": "system-matrix-dup-tool-agent",
        "session_id": ctx.session_id,
        "edge_executor_id": ctx.edge_agent_id,
        "workspace_binding": {
            "kind": "edge_workspace",
            "display_name": "system-matrix-edge",
            "root": MATRIX_E2E_EDGE_WORKSPACE_ROOT,
            "source": {
                "kind": "edge_path",
                "path": MATRIX_E2E_EDGE_WORKSPACE_ROOT
            },
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
    "execution_policy":{"turn_intent":"fixed_default","skill_auto_route":"disabled"},
        "model_selection": seeded_model_selection(ctx),
        "context": {
            "edge_profile": {
                "cwd": MATRIX_E2E_EDGE_WORKSPACE_ROOT,
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
            }]}
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
                &json!({"path":"dup.txt"}),
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
                && event["request_id"]
                    == call_id_for(&events, "read_file", |args| args["path"] == "dup.txt")
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
    assert_readonly_callback_consumption(
        ctx,
        &events,
        "read the duplicate path",
        &[(
            "tc-dup-tool-1",
            json!({"path":"dup.txt"}),
            tool_output,
            false,
        )],
    )
    .await;

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
    let fixture_model = format!("mock-{}", ctx.suffix);
    ctx.install_native_provider(&b.auth_header,vec![ProviderScript::new("run_server_stream_partial_batch_failure",move |request| request.path=="/v1/chat/completions" && request.body["model"]==fixture_model && request.body["stream"]==true && request.body["messages"].as_array().is_some_and(|messages| messages.iter().any(|message|message["role"]=="user" && message["content"]=="read two files and continue even if one fails")),vec![ProviderResponse::OpenAi(json!({"choices":[{"index":0,"message":{"role":"assistant","content":"","reasoning_content":"","tool_calls":[
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
                    ]},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":42,"completion_tokens":7,"total_tokens":49}})),ProviderResponse::OpenAi(json!({"choices":[{"index":0,"message":{"role":"assistant","content":"Handled the partial batch failure.","reasoning_content":"","tool_calls":[]},"finish_reason":"stop"}],"usage":{"prompt_tokens":42,"completion_tokens":7,"total_tokens":49}}))])]).await;
    let payload = json!({
        "agent_id": "system-matrix-partial-batch-agent",
        "session_id": ctx.session_id,
        "edge_executor_id": ctx.edge_agent_id,
        "workspace_binding": {
            "kind": "edge_workspace",
            "display_name": "system-matrix-edge",
            "root": MATRIX_E2E_EDGE_WORKSPACE_ROOT,
            "source": {
                "kind": "edge_path",
                "path": MATRIX_E2E_EDGE_WORKSPACE_ROOT
            },
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
    "execution_policy":{"turn_intent":"fixed_default","skill_auto_route":"disabled"},
        "model_selection": seeded_model_selection(ctx),
        "context": {
            "edge_profile": {
                "cwd": MATRIX_E2E_EDGE_WORKSPACE_ROOT,
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
            }]}
    });

    let req = Request::builder()
        .method("POST")
        .uri("/chat/stream")
        .header("authorization", b.auth_header.as_str())
        .header("content-type", "application/json")
        .body(Body::from(payload.to_string()))
        .expect("partial batch stream request");
    let response = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        ctx.app.clone().oneshot(req),
    )
    .await
    .expect("partial-batch chat admission did not return an SSE response in 5s")
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
    let stream_deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(12);
    loop {
        let next = tokio::time::timeout_at(stream_deadline, stream.next())
            .await
            .unwrap_or_else(|_| {
                let preview = String::from_utf8_lossy(&acc);
                panic!(
                    "partial-batch SSE did not converge in 12s; posted_first={posted_first} \
                     posted_second={posted_second} bytes={} preview={}",
                    acc.len(),
                    &preview[..preview.len().min(1_000)]
                )
            });
        let Some(chunk) = next else {
            break;
        };
        let chunk = chunk.expect("partial batch sse chunk");
        acc.extend_from_slice(&chunk);
        let s = String::from_utf8_lossy(&acc);
        if !posted_first
            && let Some(payload) = maybe_tool_result_payload_from_sse(
                s.as_ref(),
                &json!({"path":"a.txt"}),
                &ctx.edge_agent_id,
                "completed",
                ok_output,
                0,
            )
        {
            let (status, body) = tokio::time::timeout(
                std::time::Duration::from_secs(5),
                post_json(
                    &ctx.app,
                    "/tools/result",
                    Some(b.auth_header.as_str()),
                    payload,
                ),
            )
            .await
            .expect("first partial tool callback did not settle in 5s");
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
                &json!({"path":"b.txt"}),
                &ctx.edge_agent_id,
                // Edge callback statuses are a closed wire contract. Use the
                // canonical terminal failure rather than the old display-only
                // alias so this journey exercises a real failed receipt.
                "failed",
                err_output,
                0,
            )
        {
            let (status, body) = tokio::time::timeout(
                std::time::Duration::from_secs(5),
                post_json(
                    &ctx.app,
                    "/tools/result",
                    Some(b.auth_header.as_str()),
                    payload,
                ),
            )
            .await
            .expect("second partial tool callback did not settle in 5s");
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
    for request_id in [
        call_id_for(&events, "read_file", |args| args["path"] == "a.txt"),
        call_id_for(&events, "read_file", |args| args["path"] == "b.txt"),
    ] {
        let ends = events
            .iter()
            .filter(|event| {
                event.get("type").and_then(Value::as_str) == Some("tool_call_end")
                    && event.get("call_id").and_then(Value::as_str) == Some(request_id.as_str())
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
    assert_readonly_callback_consumption(
        ctx,
        &events,
        "read two files and continue even if one fails",
        &[
            ("tc-partial-1", json!({"path":"a.txt"}), ok_output, false),
            ("tc-partial-2", json!({"path":"b.txt"}), err_output, true),
        ],
    )
    .await;

    tokio::time::timeout(std::time::Duration::from_secs(8), ctx.close())
        .await
        .expect("partial-batch fixture cleanup did not settle in 8s");
}

pub async fn run_server_stream_out_of_order_tool_results() {
    let b = bootstrap().await;
    let ctx = &b.ctx;
    let first_output = "race first ok";
    let second_output = "race second ok";
    let fixture_model = format!("mock-{}", ctx.suffix);
    ctx.install_native_provider(&b.auth_header,vec![ProviderScript::new("run_server_stream_out_of_order_tool_results",move |request| request.path=="/v1/chat/completions" && request.body["model"]==fixture_model && request.body["stream"]==true && request.body["messages"].as_array().is_some_and(|messages| messages.iter().any(|message|message["role"]=="user" && message["content"]=="read two files even if callbacks arrive out of order")),vec![ProviderResponse::OpenAi(json!({"choices":[{"index":0,"message":{"role":"assistant","content":"","reasoning_content":"","tool_calls":[
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
                    ]},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":42,"completion_tokens":7,"total_tokens":49}})),ProviderResponse::OpenAi(json!({"choices":[{"index":0,"message":{"role":"assistant","content":"Handled out-of-order callback delivery.","reasoning_content":"","tool_calls":[]},"finish_reason":"stop"}],"usage":{"prompt_tokens":42,"completion_tokens":7,"total_tokens":49}}))])]).await;
    let payload = json!({
        "agent_id": "system-matrix-race-agent",
        "session_id": ctx.session_id,
        "edge_executor_id": ctx.edge_agent_id,
        "workspace_binding": {
            "kind": "edge_workspace",
            "display_name": "system-matrix-edge",
            "root": MATRIX_E2E_EDGE_WORKSPACE_ROOT,
            "source": {
                "kind": "edge_path",
                "path": MATRIX_E2E_EDGE_WORKSPACE_ROOT
            },
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
    "execution_policy":{"turn_intent":"fixed_default","skill_auto_route":"disabled"},
        "model_selection": seeded_model_selection(ctx),
        "context": {
            "edge_profile": {
                "cwd": MATRIX_E2E_EDGE_WORKSPACE_ROOT,
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
            }]}
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
                &json!({"path":"race-b.txt"}),
                &ctx.edge_agent_id,
                "completed",
                second_output,
                0,
            );
            let first_payload = maybe_tool_result_payload_from_sse(
                s.as_ref(),
                &json!({"path":"race-a.txt"}),
                &ctx.edge_agent_id,
                "completed",
                first_output,
                0,
            );
            let (Some(second_payload), Some(first_payload)) = (second_payload, first_payload)
            else {
                continue;
            };
            let second = post_json(
                &ctx.app,
                "/tools/result",
                Some(b.auth_header.as_str()),
                second_payload,
            )
            .await;
            let first = post_json(
                &ctx.app,
                "/tools/result",
                Some(b.auth_header.as_str()),
                first_payload,
            )
            .await;
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
    for request_id in [
        call_id_for(&events, "read_file", |args| args["path"] == "race-a.txt"),
        call_id_for(&events, "read_file", |args| args["path"] == "race-b.txt"),
    ] {
        let ends = events
            .iter()
            .filter(|event| {
                event.get("type").and_then(Value::as_str) == Some("tool_call_end")
                    && event.get("call_id").and_then(Value::as_str) == Some(request_id.as_str())
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
    assert_readonly_callback_consumption(
        ctx,
        &events,
        "read two files even if callbacks arrive out of order",
        &[
            (
                "tc-race-1",
                json!({"path":"race-a.txt"}),
                first_output,
                false,
            ),
            (
                "tc-race-2",
                json!({"path":"race-b.txt"}),
                second_output,
                false,
            ),
        ],
    )
    .await;

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
