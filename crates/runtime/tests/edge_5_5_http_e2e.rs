//! End-to-end: §5.5 HTTP callbacks enforce their durable interaction
//! contracts, while tool-result callbacks still use the same same-pod ledger
//! consumed by the bridge.

use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use astra_runtime::{
    AppState, AuthLoginRequestData, AuthRefreshRequestData, AuthRegisterRequestData, AuthService,
    AuthTokenRecord, AuthUserRecord, ErrorResponse, HealthChecker, ServiceInfo, build_app,
    turn::edge_ledger::{
        LEDGER_MAX_ENTRIES, expect_ledger_entry, take_ledger_entry, tool_callback_key,
    },
};
use astra_services::multi_agent::EdgeDispatchIdentity;
use astra_services::runs::{
    CancelRunRecord, ChatRequestData, ChatRunRecord, ChatStreamRecord, DurableRunInteractionKind,
    DurableRunInteractionResolveOutcome, RunLifecycleService, RunListCursor, RunListRecord,
    RunStatusRecord,
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

#[derive(Clone, Default)]
struct E2eRunLifecycle {
    runs: Arc<StdMutex<std::collections::HashMap<String, String>>>,
    required: Arc<StdMutex<std::collections::HashMap<(String, String), serde_json::Value>>>,
    resolved: Arc<StdMutex<std::collections::HashMap<(String, String), serde_json::Value>>>,
}

impl E2eRunLifecycle {
    fn add_run(&self, session_id: &str, run_id: &str) {
        self.runs
            .lock()
            .unwrap()
            .insert(run_id.to_string(), session_id.to_string());
    }

    fn add_approval_required(&self, session_id: &str, run_id: &str, request_id: &str) {
        self.add_approval_required_with_tool(
            session_id,
            run_id,
            request_id,
            "write_file",
            "standard",
        );
    }

    fn add_approval_required_with_tool(
        &self,
        session_id: &str,
        run_id: &str,
        request_id: &str,
        tool: &str,
        approval_kind: &str,
    ) {
        self.add_run(session_id, run_id);
        self.required.lock().unwrap().insert(
            (run_id.to_string(), request_id.to_string()),
            json!({
                "event_type": "approval_required",
                "data": {
                    "request_id": request_id,
                    "tool": tool,
                    "approval_kind": approval_kind,
                    "delivery": "durable",
                }
            }),
        );
    }

    fn resolved(&self, run_id: &str, request_id: &str) -> Option<serde_json::Value> {
        self.resolved
            .lock()
            .unwrap()
            .get(&(run_id.to_string(), request_id.to_string()))
            .cloned()
    }

    fn resolved_count(&self) -> usize {
        self.resolved.lock().unwrap().len()
    }
}

#[async_trait]
impl RunLifecycleService for E2eRunLifecycle {
    async fn create_run(
        &self,
        _user_id: String,
        _request: ChatRequestData,
    ) -> Result<ChatRunRecord, (StatusCode, axum::Json<ErrorResponse>)> {
        unreachable!("approval callback e2e only resolves an existing run")
    }

    async fn stream_chat(
        &self,
        _user_id: String,
        _request: ChatRequestData,
    ) -> Result<ChatStreamRecord, (StatusCode, axum::Json<ErrorResponse>)> {
        unreachable!("approval callback e2e only resolves an existing run")
    }

    async fn get_run_status(
        &self,
        run_id: String,
        user_id: String,
    ) -> Result<RunStatusRecord, (StatusCode, axum::Json<ErrorResponse>)> {
        if user_id != "e2e-user" {
            return Err((
                StatusCode::NOT_FOUND,
                axum::Json(ErrorResponse::new("run not found")),
            ));
        }
        let Some(session_id) = self.runs.lock().unwrap().get(&run_id).cloned() else {
            return Err((
                StatusCode::NOT_FOUND,
                axum::Json(ErrorResponse::new("run not found")),
            ));
        };
        Ok(RunStatusRecord {
            root_run_id: Some(run_id.clone()),
            run_id,
            session_id,
            parent_run_id: None,
            depth: 0,
            status: "waiting".into(),
            waiting_for: Some("tool_approval".into()),
            events_count: 1,
            workspace: None,
            executor: None,
            transport: None,
        })
    }

    async fn get_run_interaction_event(
        &self,
        run_id: String,
        user_id: String,
        request_id: String,
        event_type: String,
    ) -> Result<Option<serde_json::Value>, (StatusCode, axum::Json<ErrorResponse>)> {
        if user_id != "e2e-user" || event_type != "approval_required" {
            return Ok(None);
        }
        Ok(self
            .required
            .lock()
            .unwrap()
            .get(&(run_id, request_id))
            .cloned())
    }

    async fn resolve_run_interaction(
        &self,
        run_id: String,
        user_id: String,
        request_id: String,
        kind: DurableRunInteractionKind,
        response_data: serde_json::Value,
    ) -> Result<DurableRunInteractionResolveOutcome, (StatusCode, axum::Json<ErrorResponse>)> {
        if user_id != "e2e-user" || kind != DurableRunInteractionKind::Approval {
            return Ok(DurableRunInteractionResolveOutcome::MissingRequest);
        }
        if !self
            .required
            .lock()
            .unwrap()
            .contains_key(&(run_id.clone(), request_id.clone()))
        {
            return Ok(DurableRunInteractionResolveOutcome::MissingRequest);
        }
        let key = (run_id, request_id);
        let mut resolved = self.resolved.lock().unwrap();
        if let Some(existing) = resolved.get(&key) {
            return Ok(if existing.get("data") == Some(&response_data) {
                DurableRunInteractionResolveOutcome::Idempotent(existing.clone())
            } else {
                DurableRunInteractionResolveOutcome::Conflict(existing.clone())
            });
        }
        let event = json!({
            "event_type": kind.resolved_event_type(),
            "data": response_data,
        });
        resolved.insert(key, event.clone());
        Ok(DurableRunInteractionResolveOutcome::Resolved(event))
    }

    async fn stream_run(
        &self,
        _run_id: String,
        _user_id: String,
        _last_index: u32,
    ) -> Result<Vec<serde_json::Value>, (StatusCode, axum::Json<ErrorResponse>)> {
        unreachable!("approval callback e2e does not stream runs")
    }

    async fn cancel_run(
        &self,
        _run_id: String,
        _user_id: String,
    ) -> Result<CancelRunRecord, (StatusCode, axum::Json<ErrorResponse>)> {
        unreachable!("approval callback e2e does not cancel runs")
    }

    async fn list_runs_cursor(
        &self,
        _user_id: String,
        _limit: u32,
        _cursor: Option<RunListCursor>,
    ) -> Result<RunListRecord, (StatusCode, axum::Json<ErrorResponse>)> {
        unreachable!("approval callback e2e does not list runs")
    }
}

fn e2e_app_with_lifecycle(
    lifecycle: Arc<E2eRunLifecycle>,
) -> (
    Router,
    Arc<tokio::sync::Mutex<std::collections::HashMap<String, serde_json::Value>>>,
) {
    let state = AppState::new(ServiceInfo::default(), Arc::new(StubHealth))
        .with_auth_service(Arc::new(E2eAuth))
        .with_run_lifecycle_service(lifecycle);
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

fn scoped_tool_identity(
    session_id: &str,
    run_id: &str,
    turn_chain_id: &str,
    request_id: &str,
) -> EdgeDispatchIdentity {
    EdgeDispatchIdentity::new("e2e-user", session_id, run_id, turn_chain_id, request_id)
}

fn scoped_tool_key(
    session_id: &str,
    run_id: &str,
    turn_chain_id: &str,
    request_id: &str,
) -> String {
    tool_callback_key(&scoped_tool_identity(
        session_id,
        run_id,
        turn_chain_id,
        request_id,
    ))
}

fn tool_result_payload(
    session_id: &str,
    run_id: &str,
    turn_chain_id: &str,
    request_id: &str,
    output: &str,
) -> serde_json::Value {
    serde_json::to_value(astra_thin_client::ToolResultRequest::new_with_hash(
        astra_thin_client::ToolResultRequestParts {
            session_id: session_id.to_string(),
            run_id: run_id.to_string(),
            turn_chain_id: turn_chain_id.to_string(),
            request_id: request_id.to_string(),
            edge_agent_id: "edge-e2e".to_string(),
            status: "completed".to_string(),
            output: output.to_string(),
            duration_ms: 0,
            tool_result_fields: None,
        },
    ))
    .expect("tool result payload serializes")
}

#[tokio::test]
async fn post_tools_result_populates_ledger_then_take_consumes() {
    let lifecycle = Arc::new(E2eRunLifecycle::default());
    lifecycle.add_run("sess-tool", "run-tool");
    let (app, ledger) = e2e_app_with_lifecycle(lifecycle);
    let key = scoped_tool_key("sess-tool", "run-tool", "chain-tool", "tc-1");
    expect_ledger_entry(&ledger, &key);
    let (st, j) = post_json(
        app.clone(),
        "/tools/result",
        tool_result_payload("sess-tool", "run-tool", "chain-tool", "tc-1", "out"),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(j["ok"], true);
    assert_eq!(j["delivery_route"], "same_pod_ledger");
    assert!(ledger.lock().await.contains_key(&key));
    let got = take_ledger_entry(&ledger, &key, Duration::from_millis(200))
        .await
        .expect("row present");
    assert_eq!(got["kind"], "tool_result");
    assert!(ledger.lock().await.get(&key).is_none());
}

#[tokio::test]
async fn post_approval_respond_resolves_durable_interaction_without_local_ledger() {
    let lifecycle = Arc::new(E2eRunLifecycle::default());
    lifecycle.add_approval_required("sess-ledger", "run-ledger", "ap-9");
    let (app, ledger) = e2e_app_with_lifecycle(Arc::clone(&lifecycle));
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
    assert_eq!(j["durable"], true);
    assert!(ledger.lock().await.is_empty());
    let resolved = lifecycle
        .resolved("run-ledger", "ap-9")
        .expect("durable approval resolved");
    assert_eq!(resolved["event_type"], "approval_resolved");
    assert_eq!(resolved["data"]["decision"], "allow");
    assert_eq!(resolved["data"]["tool"], "write_file");
    assert_eq!(resolved["data"]["approval_kind"], "standard");
}

#[tokio::test]
async fn post_approval_respond_is_not_blocked_by_full_local_ledger() {
    let lifecycle = Arc::new(E2eRunLifecycle::default());
    lifecycle.add_approval_required("sess-approval", "run-approval", "ap-overflow");
    let (app, ledger) = e2e_app_with_lifecycle(Arc::clone(&lifecycle));
    {
        let mut guard = ledger.lock().await;
        for idx in 0..LEDGER_MAX_ENTRIES {
            guard.insert(format!("fill-{idx}"), json!({"kind": "filler"}));
        }
    }

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
    assert_eq!(j["durable"], true);
    assert_eq!(ledger.lock().await.len(), LEDGER_MAX_ENTRIES);
    let resolved = lifecycle
        .resolved("run-approval", "ap-overflow")
        .expect("durable approval resolved despite local ledger capacity");
    assert_eq!(resolved["data"]["decision"], "deny");
    assert_eq!(resolved["data"]["reason"], "policy");
}

#[tokio::test]
async fn approval_callback_on_other_appstate_uses_shared_lifecycle_without_sticky_ledger() {
    let lifecycle = Arc::new(E2eRunLifecycle::default());
    lifecycle.add_approval_required("sess-no-sticky", "run-no-sticky", "ap-no-sticky");
    let (callback_app, callback_ledger) = e2e_app_with_lifecycle(Arc::clone(&lifecycle));
    let (other_app, other_ledger) = e2e_app_with_lifecycle(Arc::clone(&lifecycle));

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
    assert!(callback_ledger.lock().await.is_empty());
    assert!(other_ledger.lock().await.is_empty());

    let (idempotent_status, idempotent_body) = post_json(
        other_app,
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
    assert_eq!(
        idempotent_status,
        StatusCode::OK,
        "shared durable state should make duplicate callback idempotent: {idempotent_body:?}"
    );
    assert_eq!(lifecycle.resolved_count(), 1);
}

#[test]
fn concurrent_duplicate_approval_responses_resolve_once_durably() {
    let lifecycle = Arc::new(E2eRunLifecycle::default());
    lifecycle.add_approval_required(
        "sess-concurrent-dup",
        "run-concurrent-dup",
        "ap-concurrent-dup",
    );
    let (app, ledger) = e2e_app_with_lifecycle(Arc::clone(&lifecycle));
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
        let first_payload = payload.clone();
        let first = scope.spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(post_json(first_app, "/approval/respond", first_payload))
        });

        let second_app = app.clone();
        let second_payload = payload.clone();
        let second = scope.spawn(move || {
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

    assert_eq!(lifecycle.resolved_count(), 1);
    assert!(ledger.blocking_lock().is_empty());
}

/// Complement of the identical-payload idempotency test above: when two
/// approval callbacks arrive for the same `request_id` with *different*
/// decisions, the second one must be rejected with 409 CONFLICT so a
/// delayed/malicious replay cannot flip an already-recorded decision.
#[tokio::test]
async fn distinct_payload_duplicate_approval_second_is_409_conflict() {
    let lifecycle = Arc::new(E2eRunLifecycle::default());
    lifecycle.add_approval_required("sess-conflict", "run-conflict", "ap-conflict");
    let (app, ledger) = e2e_app_with_lifecycle(Arc::clone(&lifecycle));

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

    let stored = lifecycle
        .resolved("run-conflict", "ap-conflict")
        .expect("first approval stored");
    assert_eq!(
        stored["data"]["decision"], "allow",
        "original durable decision must not be overwritten: {stored:?}"
    );
    assert!(ledger.lock().await.is_empty());
}

#[tokio::test]
async fn post_tool_result_rejects_when_ledger_is_full() {
    let lifecycle = Arc::new(E2eRunLifecycle::default());
    lifecycle.add_run("sess-overflow", "run-overflow");
    let (app, ledger) = e2e_app_with_lifecycle(lifecycle);
    {
        let mut guard = ledger.lock().await;
        for idx in 0..LEDGER_MAX_ENTRIES {
            guard.insert(format!("fill-{idx}"), json!({"kind": "filler"}));
        }
    }

    let key = scoped_tool_key(
        "sess-overflow",
        "run-overflow",
        "chain-overflow",
        "tc-overflow",
    );
    expect_ledger_entry(&ledger, &key);
    let (st, _) = post_json(
        app.clone(),
        "/tools/result",
        tool_result_payload(
            "sess-overflow",
            "run-overflow",
            "chain-overflow",
            "tc-overflow",
            "out",
        ),
    )
    .await;
    assert_eq!(st, StatusCode::SERVICE_UNAVAILABLE);
    assert!(!ledger.lock().await.contains_key(&key));
    assert_eq!(ledger.lock().await.len(), LEDGER_MAX_ENTRIES);
}

#[tokio::test]
async fn http_handler_payload_matches_delivery_parser() {
    let lifecycle = Arc::new(E2eRunLifecycle::default());
    lifecycle.add_approval_required_with_tool(
        "sess-chain",
        "run-chain",
        "w-chain",
        "write_file",
        "standard",
    );
    let (app, ledger) = e2e_app_with_lifecycle(Arc::clone(&lifecycle));
    let (status, body) = post_json(
        app.clone(),
        "/approval/respond",
        json!({
            "request_id": "w-chain",
            "decision": "allow",
            "tool_name": "write_file",
            "approval_kind": "standard",
            "session_id": "sess-chain",
            "run_id": "run-chain"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body:?}");
    assert!(ledger.lock().await.is_empty());
    let resolved = lifecycle
        .resolved("run-chain", "w-chain")
        .expect("approval response resolved");
    assert_eq!(resolved["data"]["request_id"], "w-chain");
    assert_eq!(resolved["data"]["decision"], "allow");
    assert_eq!(resolved["data"]["tool"], "write_file");
    assert_eq!(resolved["data"]["approval_kind"], "standard");
}

#[tokio::test]
async fn http_handler_payload_supports_batched_approval_delivery() {
    let lifecycle = Arc::new(E2eRunLifecycle::default());
    for request_id in ["w-batch-1", "w-batch-2"] {
        lifecycle.add_approval_required("sess-batch", "run-batch", request_id);
    }
    let (app, ledger) = e2e_app_with_lifecycle(Arc::clone(&lifecycle));
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
    assert_eq!(lifecycle.resolved_count(), 2);
    assert!(ledger.lock().await.is_empty());
    for request_id in ["w-batch-1", "w-batch-2"] {
        assert_eq!(
            lifecycle.resolved("run-batch", request_id).unwrap()["data"]["decision"],
            "allow"
        );
    }
}
