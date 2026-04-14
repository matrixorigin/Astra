//! End-to-end: §5.5 HTTP handlers write into the same ledger the bridge consumes, keys match
//! [`astra_runtime::turn::edge_ledger`], and `take_ledger_entry` removes rows.

use std::sync::Arc;
use std::time::Duration;

use astra_runtime::{
    AppState, AuthLoginRequestData, AuthRefreshRequestData, AuthRegisterRequestData, AuthService,
    AuthTokenRecord, AuthUserRecord, ErrorResponse, HealthChecker, ServiceInfo, build_app,
    turn::cloud_tool_delivery::deliver_tool_calls_through_edge_ledger,
    turn::edge_ledger::{approval_callback_key, take_ledger_entry, tool_callback_key},
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
