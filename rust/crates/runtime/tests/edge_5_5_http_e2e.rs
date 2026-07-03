//! End-to-end: §5.5 HTTP handlers write into the same ledger the bridge consumes, keys match
//! [`astra_turn_core::edge_ledger`], and `take_ledger_entry` removes rows.

use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use astra_runtime::{
    AppState, AuthLoginRequestData, AuthRefreshRequestData, AuthRegisterRequestData, AuthService,
    AuthTokenRecord, AuthUserRecord, ErrorResponse, HealthChecker, ServiceInfo, build_app,
    turn::cloud_tool_delivery::{
        ApprovalAuditContext, deliver_tool_calls_through_edge_ledger_with_approval_audit,
        wait_approval_ledger_for_tool,
    },
    turn::edge_ledger::{
        LEDGER_MAX_ENTRIES, approval_callback_key, take_ledger_entry, tool_callback_key,
    },
};
use astra_services::session_journal::{
    JournalDirGuard, JournalEventType, find_latest_approval_decision_for_run, read_journal,
};
use astra_turn_core::contracts::{TurnAuxiliaryEventRecord, TurnAuxiliaryEventWriter};
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

#[derive(Clone, Default)]
struct RecordingAuxiliaryEventWriter {
    events: Arc<StdMutex<Vec<TurnAuxiliaryEventRecord>>>,
}

#[async_trait]
impl TurnAuxiliaryEventWriter for RecordingAuxiliaryEventWriter {
    async fn persist_events(&self, events: Vec<TurnAuxiliaryEventRecord>) -> Result<(), String> {
        self.events.lock().unwrap().extend(events);
        Ok(())
    }
}

fn approval_audit(session_id: &str, run_id: &str) -> ApprovalAuditContext {
    ApprovalAuditContext {
        user_id: "e2e-user".to_string(),
        session_id: session_id.to_string(),
        run_id: run_id.to_string(),
        turn: 3,
        agent_id: None,
        parent_event_id: None,
        parent_event_ids: Vec::new(),
        causal_chain_id: format!("chain-{run_id}"),
        auxiliary_event_writer: Arc::new(RecordingAuxiliaryEventWriter::default()),
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

fn write_tool_call(id: &str) -> serde_json::Value {
    json!({
        "id": id,
        "type": "function",
        "function": {"name": "write_file", "arguments": r#"{"path":"z.rs","content":"1"}"#}
    })
}

#[tokio::test]
async fn post_tools_result_populates_ledger_then_take_consumes() {
    let (app, ledger) = e2e_app();
    let key = tool_callback_key("e2e-user", "tc-1");
    let (st, j) = post_json(
        app.clone(),
        "/tools/result",
        json!({
            "request_id": "tc-1",
            "status": "completed",
            "output": "out",
            "result_hash": astra_thin_client::ToolResultRequest::compute_result_hash("tc-1", "out"),
        }),
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
    let key = approval_callback_key("e2e-user", "sess-ledger", "run-ledger", "ap-9");
    let (st, j) = post_json(
        app.clone(),
        "/approval/respond",
        json!({
            "request_id": "ap-9",
            "decision": "allow",
            "session_id": "sess-ledger",
            "run_id": "run-ledger"
        }),
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

    let key = approval_callback_key("e2e-user", "sess-approval", "run-approval", "ap-overflow");
    let (st, j) = post_json(
        app.clone(),
        "/approval/respond",
        json!({
            "request_id": "ap-overflow",
            "decision": "deny",
            "reason": "policy",
            "session_id": "sess-approval",
            "run_id": "run-approval",
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

    let found =
        find_latest_approval_decision_for_run("sess-approval", "ap-overflow", "run-approval")
            .unwrap()
            .expect("journal decision");
    assert_eq!(found.run_id.as_deref(), Some("run-approval"));
    assert_eq!(found.decision, "deny");
    assert_eq!(found.reason.as_deref(), Some("policy"));
    assert_eq!(found.tool_name.as_deref(), Some("write_file"));
    assert_eq!(found.approval_kind.as_deref(), Some("standard"));
}

#[tokio::test]
async fn approval_callback_on_other_appstate_replays_from_journal_without_sticky_ledger() {
    let temp = tempfile::tempdir().unwrap();
    let _guard = JournalDirGuard::new(temp.path());
    let (callback_app, callback_ledger) = e2e_app();
    let (_waiter_app, waiter_ledger) = e2e_app();

    let (st, j) = post_json(
        callback_app,
        "/approval/respond",
        json!({
            "request_id": "ap-no-sticky",
            "decision": "allow",
            "reason": "approved on callback pod",
            "session_id": "sess-no-sticky",
            "run_id": "run-no-sticky",
            "tool_name": "write_file",
            "approval_kind": "standard"
        }),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "callback pod approval response: {j:?}");
    assert_eq!(j["ok"], true);
    assert!(
        callback_ledger
            .lock()
            .await
            .contains_key(&approval_callback_key(
                "e2e-user",
                "sess-no-sticky",
                "run-no-sticky",
                "ap-no-sticky"
            )),
        "callback pod may keep its same-pod fast-path ledger entry"
    );
    assert!(
        waiter_ledger.lock().await.is_empty(),
        "waiter pod intentionally has no same-pod ledger entry"
    );

    let aux_writer = RecordingAuxiliaryEventWriter::default();
    let recorded_events = Arc::clone(&aux_writer.events);
    let audit = ApprovalAuditContext {
        user_id: "e2e-user".to_string(),
        session_id: "sess-no-sticky".to_string(),
        run_id: "run-no-sticky".to_string(),
        turn: 3,
        agent_id: None,
        parent_event_id: None,
        parent_event_ids: Vec::new(),
        causal_chain_id: "chain-no-sticky".to_string(),
        auxiliary_event_writer: Arc::new(aux_writer),
    };

    wait_approval_ledger_for_tool(
        &waiter_ledger,
        "e2e-user",
        &write_tool_call("ap-no-sticky"),
        Duration::from_millis(200),
        Some(&audit),
    )
    .await
    .expect("waiter pod should replay approval decision from journal without sticky ledger");

    assert!(waiter_ledger.lock().await.is_empty());
    let events = recorded_events.lock().unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].event_type, "approval_decision");
    let metadata = events[0].metadata.as_ref().expect("approval metadata");
    assert_eq!(metadata["outcome_source"].as_str(), Some("journal"));
    assert_eq!(metadata["decision"].as_str(), Some("allow"));
    assert_eq!(metadata["run_id"].as_str(), Some("run-no-sticky"));
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
        "run_id": "run-concurrent-dup",
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
        // Post-Phase-R fix: identical-payload duplicate callbacks are
        // treated as idempotent HTTP retries — both return 200. The
        // at-most-once ledger still records the value exactly once, and
        // the journal still writes exactly one approval decision.
        // (A *distinct-payload* duplicate would return 409 and is
        // covered by `concurrent_distinct_payload_approval_conflicts_on_second`.)
        assert_eq!(
            first_status,
            StatusCode::OK,
            "identical-payload duplicate should be idempotent 200: {first_body:?}"
        );
        assert_eq!(
            second_status,
            StatusCode::OK,
            "identical-payload duplicate should be idempotent 200: {second_body:?}"
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
                && event
                    .metadata
                    .as_ref()
                    .and_then(|metadata| metadata.get("approval"))
                    .and_then(|approval| approval.get("run_id"))
                    .and_then(|run_id| run_id.as_str())
                    == Some("run-concurrent-dup")
        })
        .count();
    assert_eq!(
        approval_decisions, 1,
        "concurrent duplicate approvals should record one approval decision"
    );

    let key = approval_callback_key(
        "e2e-user",
        "sess-concurrent-dup",
        "run-concurrent-dup",
        "ap-concurrent-dup",
    );
    assert!(ledger.blocking_lock().contains_key(&key));
}

/// Complement of the identical-payload idempotency test above: when two
/// approval callbacks arrive for the same `request_id` with *different*
/// decisions, the second one must be rejected with 409 CONFLICT so a
/// delayed/malicious replay cannot flip an already-recorded decision.
#[tokio::test]
async fn distinct_payload_duplicate_approval_second_is_409_conflict() {
    let temp = tempfile::tempdir().unwrap();
    let _guard = JournalDirGuard::new(temp.path());
    let (app, ledger) = e2e_app();

    let (st1, j1) = post_json(
        app.clone(),
        "/approval/respond",
        json!({
            "request_id": "ap-conflict",
            "decision": "allow",
            "reason": "first",
            "session_id": "sess-conflict",
            "run_id": "run-conflict",
            "tool_name": "write_file",
            "approval_kind": "standard"
        }),
    )
    .await;
    assert_eq!(st1, StatusCode::OK);
    assert_eq!(j1["ok"], true);

    let (st2, j2) = post_json(
        app.clone(),
        "/approval/respond",
        json!({
            "request_id": "ap-conflict",
            "decision": "deny",
            "reason": "second",
            "session_id": "sess-conflict",
            "run_id": "run-conflict",
            "tool_name": "write_file",
            "approval_kind": "standard"
        }),
    )
    .await;
    assert_eq!(
        st2,
        StatusCode::CONFLICT,
        "distinct-payload duplicate must return 409: {j2:?}"
    );

    let key = approval_callback_key("e2e-user", "sess-conflict", "run-conflict", "ap-conflict");
    let stored = ledger
        .lock()
        .await
        .get(&key)
        .cloned()
        .expect("first approval stored");
    assert_eq!(
        stored["body"]["decision"], "allow",
        "original ledger value must not be overwritten: {stored:?}"
    );
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
        json!({
            "request_id": "tc-overflow",
            "status": "completed",
            "output": "out",
            "result_hash": astra_thin_client::ToolResultRequest::compute_result_hash("tc-overflow", "out"),
        }),
    )
    .await;
    assert_eq!(st, StatusCode::SERVICE_UNAVAILABLE);
    assert!(!ledger.lock().await.contains_key(&key));
    assert_eq!(ledger.lock().await.len(), LEDGER_MAX_ENTRIES);
}

#[tokio::test]
async fn http_handler_payload_matches_delivery_parser() {
    let (app, ledger) = e2e_app();
    let (status, body) = post_json(
        app.clone(),
        "/approval/respond",
        json!({
            "request_id": "w-chain",
            "decision": "allow",
            "session_id": "sess-chain",
            "run_id": "run-chain"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body:?}");
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
                json!({"body": {"request_id": "w-chain", "status": "completed", "output": "ok"}}),
            );
        }
    });
    let audit = approval_audit("sess-chain", "run-chain");
    let d = deliver_tool_calls_through_edge_ledger_with_approval_audit(
        &ledger,
        "e2e-user",
        &[tc],
        Duration::from_secs(2),
        Some(&audit),
    )
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
        let (status, body) = post_json(
            app.clone(),
            "/approval/respond",
            json!({
                "request_id": request_id,
                "decision": "allow",
                "session_id": "sess-batch",
                "run_id": "run-batch"
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body:?}");
    }
    tokio::spawn({
        let ledger = ledger.clone();
        async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            let mut guard = ledger.lock().await;
            guard.insert(
                tool_callback_key("e2e-user", "w-batch-1"),
                json!({"body": {"request_id": "w-batch-1", "status": "completed", "output": "ok-1"}}),
            );
            guard.insert(
                tool_callback_key("e2e-user", "w-batch-2"),
                json!({"body": {"request_id": "w-batch-2", "status": "completed", "output": "ok-2"}}),
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
    let audit = approval_audit("sess-batch", "run-batch");
    let d = deliver_tool_calls_through_edge_ledger_with_approval_audit(
        &ledger,
        "e2e-user",
        &tcs,
        Duration::from_secs(2),
        Some(&audit),
    )
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
