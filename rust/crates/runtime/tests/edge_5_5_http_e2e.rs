//! End-to-end: §5.5 HTTP handlers write into the same ledger the bridge consumes, keys match
//! [`astra_runtime::turn::edge_ledger`], and `take_ledger_entry` removes rows.

use std::sync::Arc;
use std::time::Duration;

use astra_runtime::{
    AppState, AuthLoginRequestData, AuthRefreshRequestData, AuthRegisterRequestData, AuthService,
    AuthTokenRecord, AuthUserRecord, ErrorResponse, HealthChecker, ServiceInfo, build_app,
    turn::cloud_tool_delivery::deliver_tool_calls_through_edge_ledger,
    turn::edge_ledger::{
        LEDGER_MAX_ENTRIES, approval_callback_key, take_ledger_entry, tool_callback_key,
    },
};
use astra_services::session_journal::{
    JournalDirGuard, JournalEventType, find_latest_approval_decision, read_journal,
};
use async_trait::async_trait;
use axum::{
    Router, body,
    http::{HeaderMap, Request, StatusCode},
};
use serde_json::json;
use tower::util::ServiceExt;

#[derive(Clone)]
struct StubHealth;

#[async_trait]
impl HealthChecker for StubHealth {
    async fn database_healthy(&self) -> bool {
        true
    }
}

#[derive(Clone)]
struct E2eAuth;

#[async_trait]
impl AuthService for E2eAuth {
    async fn register(
        &self,
        _request: AuthRegisterRequestData,
    ) -> Result<AuthUserRecord, (StatusCode, axum::Json<ErrorResponse>)> {
        Err((
            StatusCode::NOT_IMPLEMENTED,
            axum::Json(ErrorResponse::new("e2e stub")),
        ))
    }

    async fn login(
        &self,
        _request: AuthLoginRequestData,
    ) -> Result<AuthTokenRecord, (StatusCode, axum::Json<ErrorResponse>)> {
        Err((
            StatusCode::NOT_IMPLEMENTED,
            axum::Json(ErrorResponse::new("e2e stub")),
        ))
    }

    async fn refresh(
        &self,
        _request: AuthRefreshRequestData,
    ) -> Result<AuthTokenRecord, (StatusCode, axum::Json<ErrorResponse>)> {
        Err((
            StatusCode::NOT_IMPLEMENTED,
            axum::Json(ErrorResponse::new("e2e stub")),
        ))
    }

    async fn logout(
        &self,
        _request: AuthRefreshRequestData,
    ) -> Result<(), (StatusCode, axum::Json<ErrorResponse>)> {
        Err((
            StatusCode::NOT_IMPLEMENTED,
            axum::Json(ErrorResponse::new("e2e stub")),
        ))
    }

    async fn current_user(
        &self,
        headers: &HeaderMap,
    ) -> Result<AuthUserRecord, (StatusCode, axum::Json<ErrorResponse>)> {
        let ok =
            headers.get("authorization").and_then(|v| v.to_str().ok()) == Some("Bearer e2e-token");
        if ok {
            Ok(AuthUserRecord {
                user_id: "e2e-user".to_string(),
                username: "e2e".to_string(),
                email: "e@e.e".to_string(),
                display_name: None,
            })
        } else {
            Err((
                StatusCode::UNAUTHORIZED,
                axum::Json(ErrorResponse::new("bad token")),
            ))
        }
    }
}

fn e2e_app() -> (
    Router,
    Arc<tokio::sync::Mutex<std::collections::HashMap<String, serde_json::Value>>>,
) {
    let state = AppState::new(ServiceInfo::default(), Arc::new(StubHealth))
        .with_auth_service(Arc::new(E2eAuth));
    let ledger = state.edge_callback_ledger();
    let app = build_app(state);
    (app, ledger)
}

fn post_request(path: &str, body: serde_json::Value) -> Request<body::Body> {
    Request::builder()
        .method("POST")
        .uri(path)
        .header("authorization", "Bearer e2e-token")
        .header("content-type", "application/json")
        .body(body::Body::from(body.to_string()))
        .unwrap()
}

async fn post_json(
    app: Router,
    path: &str,
    payload: serde_json::Value,
) -> (StatusCode, serde_json::Value) {
    let response = app.oneshot(post_request(path, payload)).await.unwrap();
    let status = response.status();
    let bytes = body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json = serde_json::from_slice(&bytes).unwrap_or(json!({}));
    (status, json)
}

#[tokio::test]
async fn post_tools_result_populates_ledger_then_take_consumes() {
    let (app, ledger) = e2e_app();
    let key = tool_callback_key("e2e-user", "tc-1");
    let (st, j) = post_json(
        app.clone(),
        "/tools/result",
        json!({"request_id": "tc-1", "status": "ok", "output": "out"}),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(j["ok"], true);
    assert!(ledger.lock().await.contains_key(&key));
    let got = take_ledger_entry(&ledger, &key, Duration::from_millis(200))
        .await
        .expect("row present");
    assert_eq!(got["kind"], "tool_result");
    assert!(ledger.lock().await.get(&key).is_none());
}

#[tokio::test]
async fn post_approval_respond_populates_ledger_then_take_consumes() {
    let (app, ledger) = e2e_app();
    let key = approval_callback_key("e2e-user", "ap-9");
    let (st, j) = post_json(
        app.clone(),
        "/approval/respond",
        json!({"request_id": "ap-9", "decision": "allow"}),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(j["ok"], true);
    assert!(ledger.lock().await.contains_key(&key));
    let got = take_ledger_entry(&ledger, &key, Duration::from_millis(200))
        .await
        .expect("row present");
    assert_eq!(got["kind"], "approval_respond");
}

#[tokio::test]
async fn post_approval_respond_journals_when_ledger_is_full() {
    let temp = tempfile::tempdir().unwrap();
    let _guard = JournalDirGuard::new(temp.path());
    let (app, ledger) = e2e_app();
    {
        let mut guard = ledger.lock().await;
        for idx in 0..LEDGER_MAX_ENTRIES {
            guard.insert(format!("fill-{idx}"), json!({"kind": "filler"}));
        }
    }

    let key = approval_callback_key("e2e-user", "ap-overflow");
    let (st, j) = post_json(
        app.clone(),
        "/approval/respond",
        json!({
            "request_id": "ap-overflow",
            "decision": "deny",
            "reason": "policy",
            "session_id": "sess-approval",
            "tool_name": "write_file",
            "approval_kind": "standard"
        }),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(j["ok"], true);
    assert_eq!(j["ledger_enqueued"], false);
    assert!(!ledger.lock().await.contains_key(&key));
    assert_eq!(ledger.lock().await.len(), LEDGER_MAX_ENTRIES);

    let found = find_latest_approval_decision("sess-approval", "ap-overflow")
        .unwrap()
        .expect("journal decision");
    assert_eq!(found.decision, "deny");
    assert_eq!(found.reason.as_deref(), Some("policy"));
    assert_eq!(found.tool_name.as_deref(), Some("write_file"));
    assert_eq!(found.approval_kind.as_deref(), Some("standard"));
}

#[test]
fn concurrent_duplicate_approval_responses_record_one_journal_decision() {
    let temp = tempfile::tempdir().unwrap();
    let journal_dir = temp.path().to_path_buf();
    let (app, ledger) = e2e_app();
    let payload = json!({
        "request_id": "ap-concurrent-dup",
        "decision": "allow",
        "reason": "duplicate allow",
        "session_id": "sess-concurrent-dup",
        "tool_name": "write_file",
        "approval_kind": "standard"
    });

    std::thread::scope(|scope| {
        let first_app = app.clone();
        let first_dir = journal_dir.clone();
        let first_payload = payload.clone();
        let first = scope.spawn(move || {
            let _guard = JournalDirGuard::new(&first_dir);
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(post_json(first_app, "/approval/respond", first_payload))
        });

        let second_app = app.clone();
        let second_dir = journal_dir.clone();
        let second_payload = payload.clone();
        let second = scope.spawn(move || {
            let _guard = JournalDirGuard::new(&second_dir);
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(post_json(second_app, "/approval/respond", second_payload))
        });

        let (first_status, first_body) = first.join().unwrap();
        let (second_status, second_body) = second.join().unwrap();
        // Post-Phase-R fix: the ledger insert is at-most-once, so exactly
        // one of the two concurrent callers sees 200 OK and the other
        // sees 409 CONFLICT (refusal to overwrite). Journal idempotency
        // still guarantees a single recorded decision regardless (asserted
        // below).
        let statuses = [
            (first_status, first_body.clone()),
            (second_status, second_body.clone()),
        ];
        let ok_count = statuses
            .iter()
            .filter(|(s, _)| *s == StatusCode::OK)
            .count();
        let conflict_count = statuses
            .iter()
            .filter(|(s, _)| *s == StatusCode::CONFLICT)
            .count();
        assert_eq!(
            ok_count, 1,
            "exactly one concurrent duplicate approval returns 200: {statuses:?}"
        );
        assert_eq!(
            conflict_count, 1,
            "the other duplicate approval returns 409 CONFLICT: {statuses:?}"
        );
    });

    let _guard = JournalDirGuard::new(&journal_dir);
    let approval_decisions = read_journal("sess-concurrent-dup")
        .unwrap()
        .into_iter()
        .filter(|event| event.event_type == JournalEventType::ApprovalDecision)
        .filter(|event| {
            event
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.get("approval"))
                .and_then(|approval| approval.get("request_id"))
                .and_then(|request_id| request_id.as_str())
                == Some("ap-concurrent-dup")
        })
        .count();
    assert_eq!(
        approval_decisions, 1,
        "concurrent duplicate approvals should record one approval decision"
    );

    let key = approval_callback_key("e2e-user", "ap-concurrent-dup");
    assert!(ledger.blocking_lock().contains_key(&key));
}

#[tokio::test]
async fn post_tool_result_rejects_when_ledger_is_full() {
    let (app, ledger) = e2e_app();
    {
        let mut guard = ledger.lock().await;
        for idx in 0..LEDGER_MAX_ENTRIES {
            guard.insert(format!("fill-{idx}"), json!({"kind": "filler"}));
        }
    }

    let key = tool_callback_key("e2e-user", "tc-overflow");
    let (st, _) = post_json(
        app.clone(),
        "/tools/result",
        json!({"request_id": "tc-overflow", "status": "ok", "output": "out"}),
    )
    .await;
    assert_eq!(st, StatusCode::SERVICE_UNAVAILABLE);
    assert!(!ledger.lock().await.contains_key(&key));
    assert_eq!(ledger.lock().await.len(), LEDGER_MAX_ENTRIES);
}

#[tokio::test]
async fn http_handler_payload_matches_delivery_parser() {
    let (app, ledger) = e2e_app();
    post_json(
        app.clone(),
        "/approval/respond",
        json!({"request_id": "w-chain", "decision": "allow"}),
    )
    .await;
    let tc = json!({
        "id": "w-chain",
        "type": "function",
        "function": {"name": "write_file", "arguments": r#"{"path":"z.rs","content":"1"}"#}
    });
    tokio::spawn({
        let ledger = ledger.clone();
        async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            ledger.lock().await.insert(
                tool_callback_key("e2e-user", "w-chain"),
                json!({"body": {"request_id": "w-chain", "status": "ok", "output": "ok"}}),
            );
        }
    });
    let d =
        deliver_tool_calls_through_edge_ledger(&ledger, "e2e-user", &[tc], Duration::from_secs(2))
            .await;
    let approval = d
        .sse_maps
        .iter()
        .find(|m| m.get("type").and_then(|v| v.as_str()) == Some("approval_required"))
        .expect("approval_required event");
    assert_eq!(
        approval.get("approval_kind").and_then(|v| v.as_str()),
        Some("standard")
    );
    assert!(
        d.sse_maps
            .iter()
            .any(|m| m.get("type").and_then(|v| v.as_str()) == Some("tool_request"))
    );
    assert!(
        d.tool_messages[0]["content"]
            .as_str()
            .unwrap()
            .contains("ok")
    );
}

#[tokio::test]
async fn http_handler_payload_supports_batched_approval_delivery() {
    let (app, ledger) = e2e_app();
    for request_id in ["w-batch-1", "w-batch-2"] {
        post_json(
            app.clone(),
            "/approval/respond",
            json!({"request_id": request_id, "decision": "allow"}),
        )
        .await;
    }
    tokio::spawn({
        let ledger = ledger.clone();
        async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            let mut guard = ledger.lock().await;
            guard.insert(
                tool_callback_key("e2e-user", "w-batch-1"),
                json!({"body": {"request_id": "w-batch-1", "status": "ok", "output": "ok-1"}}),
            );
            guard.insert(
                tool_callback_key("e2e-user", "w-batch-2"),
                json!({"body": {"request_id": "w-batch-2", "status": "ok", "output": "ok-2"}}),
            );
        }
    });
    let tcs = vec![
        json!({
            "id": "w-batch-1",
            "type": "function",
            "function": {"name": "write_file", "arguments": r#"{"path":"a.rs","content":"1"}"#}
        }),
        json!({
            "id": "w-batch-2",
            "type": "function",
            "function": {"name": "write_file", "arguments": r#"{"path":"b.rs","content":"2"}"#}
        }),
    ];
    let d =
        deliver_tool_calls_through_edge_ledger(&ledger, "e2e-user", &tcs, Duration::from_secs(2))
            .await;

    assert!(
        d.sse_maps
            .iter()
            .all(|m| m.get("type").and_then(|v| v.as_str()) != Some("approval_required"))
    );
    let batch = d
        .sse_maps
        .iter()
        .find(|m| m.get("type").and_then(|v| v.as_str()) == Some("approval_batch_required"))
        .expect("approval_batch_required event");
    assert_eq!(batch["requests"].as_array().unwrap().len(), 2);

    let tool_request_positions: Vec<_> = d
        .sse_maps
        .iter()
        .enumerate()
        .filter_map(|(idx, m)| {
            (m.get("type").and_then(|v| v.as_str()) == Some("tool_request")).then_some(idx)
        })
        .collect();
    assert_eq!(tool_request_positions.len(), 2);
    let first_end = d
        .sse_maps
        .iter()
        .position(|m| m.get("type").and_then(|v| v.as_str()) == Some("tool_call_end"))
        .expect("tool_call_end");
    assert!(tool_request_positions.iter().all(|idx| *idx < first_end));
}
