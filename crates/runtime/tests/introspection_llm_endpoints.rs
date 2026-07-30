//! Contract: three new LLM-facing introspection endpoints expose the
//! runtime's own decision / tool / drift state as stable JSON the agent can
//! parse in a tool call.
//!
//! - `GET /introspection/decision-trace?session_id=...&last_n=20`
//! - `GET /introspection/tool-history?tool=...&window_hours=24`
//! - `GET /introspection/drift-check?session_id=...`
//!
//! Uses a stub `IntrospectionService` that returns canned JSON, so it pins
//! the *HTTP layer's* contract (schema, auth gating, query defaults,
//! error surface) without touching MatrixOne. Live-DB coverage stays in
//! the `system_matrix_http_e2e` journey.

use std::sync::Arc;

use async_trait::async_trait;
use axum::{
    Json, Router,
    body::Body,
    http::{HeaderMap, Request, StatusCode},
    routing::get,
};
use serde_json::{Value, json};
use tower::util::ServiceExt;

use astra_runtime::{
    AppState, AuthLoginRequestData, AuthRefreshRequestData, AuthRegisterRequestData, AuthService,
    AuthTokenRecord, AuthUserRecord, ErrorResponse, HealthChecker, ServiceInfo,
    introspection::{
        get_decision_trace_handler, get_drift_check_handler, get_tool_history_handler,
    },
};
use astra_services::introspection::{
    INTENT_DRIFT_ASSESSMENT_SCHEMA_VERSION, IntentDriftAssessmentProvenance,
    IntentDriftAssessmentProvenanceKind, IntentDriftAssessmentV1, IntentDriftCheckResponseV2,
    IntentDriftLevel, IntentDriftVerdict, IntrospectionService, ServiceResult,
    SkillsIntrospectionResponse,
};

// ─── Stubs ──────────────────────────────────────────────────────────────────

#[derive(Clone)]
struct StubHealth;

#[async_trait]
impl HealthChecker for StubHealth {
    async fn database_healthy(&self) -> bool {
        true
    }
}

struct StubAuth;

#[async_trait]
impl AuthService for StubAuth {
    async fn register(
        &self,
        _r: AuthRegisterRequestData,
    ) -> Result<AuthUserRecord, (StatusCode, Json<ErrorResponse>)> {
        unreachable!()
    }
    async fn login(
        &self,
        _r: AuthLoginRequestData,
    ) -> Result<AuthTokenRecord, (StatusCode, Json<ErrorResponse>)> {
        unreachable!()
    }
    async fn refresh(
        &self,
        _r: AuthRefreshRequestData,
    ) -> Result<AuthTokenRecord, (StatusCode, Json<ErrorResponse>)> {
        unreachable!()
    }
    async fn logout(
        &self,
        _r: AuthRefreshRequestData,
    ) -> Result<(), (StatusCode, Json<ErrorResponse>)> {
        unreachable!()
    }
    async fn current_user(
        &self,
        headers: &HeaderMap,
    ) -> Result<AuthUserRecord, (StatusCode, Json<ErrorResponse>)> {
        match headers.get("authorization").and_then(|v| v.to_str().ok()) {
            Some(h) if h.starts_with("Bearer ") => {
                let user_id = h.trim_start_matches("Bearer ");
                Ok(AuthUserRecord {
                    user_id: user_id.to_string(),
                    username: format!("user-{user_id}"),
                    email: format!("{user_id}@test.local"),
                    display_name: None,
                })
            }
            _ => Err((
                StatusCode::UNAUTHORIZED,
                Json(ErrorResponse::new("Not authenticated")),
            )),
        }
    }
}

struct StubIntrospection;

#[async_trait]
impl IntrospectionService for StubIntrospection {
    async fn get_skills_introspection(
        &self,
        _: &str,
    ) -> ServiceResult<SkillsIntrospectionResponse> {
        unreachable!("not called by this test")
    }
    async fn get_context_trend(&self, _: &str, _: &str, _: i32, _: i64) -> ServiceResult<Value> {
        unreachable!("not called by this test")
    }
    async fn get_context_snapshot(
        &self,
        _: &str,
        _: &str,
        _: Option<i32>,
        _: bool,
        _: bool,
        _: i32,
    ) -> ServiceResult<Value> {
        unreachable!("not called by this test")
    }
    async fn get_retrieval_quality(&self, _: &str, _: &str, _: i32) -> ServiceResult<Value> {
        unreachable!("not called by this test")
    }

    async fn get_decision_trace(
        &self,
        user_id: &str,
        session_id: &str,
        last_n: i32,
    ) -> ServiceResult<Value> {
        Ok(json!({
            "schema_version": 1,
            "session_id": session_id,
            "user_id": user_id,
            "last_n": last_n,
            "decisions": [
                {
                    "decision_id": "d1",
                    "decision_type": "tool_surface",
                    "created_at": "2026-05-01T10:00:00",
                    "output": {"tool": "grep"},
                },
            ],
        }))
    }

    async fn get_tool_history(
        &self,
        user_id: &str,
        tool: &str,
        window_hours: i32,
    ) -> ServiceResult<Value> {
        Ok(json!({
            "schema_version": 1,
            "user_id": user_id,
            "tool": tool,
            "window_hours": window_hours,
            "total_calls": 7,
            "ok_count": 5,
            "fail_count": 2,
            "success_rate": 5.0 / 7.0,
            "recent_failures": [],
        }))
    }

    async fn get_drift_check(
        &self,
        user_id: &str,
        session_id: &str,
    ) -> ServiceResult<IntentDriftCheckResponseV2> {
        Ok(IntentDriftCheckResponseV2::assessed(
            user_id,
            session_id,
            IntentDriftAssessmentV1 {
                schema_version: INTENT_DRIFT_ASSESSMENT_SCHEMA_VERSION,
                provenance: IntentDriftAssessmentProvenance {
                    kind: IntentDriftAssessmentProvenanceKind::LlmJudge,
                    invocation_id: "invocation-1".into(),
                    provider: "test-provider".into(),
                    model: "test-model".into(),
                    provider_response_id: None,
                },
                verdict: IntentDriftVerdict::Drifting,
                score: 0.42,
                level: IntentDriftLevel::Moderate,
                evidence: vec!["the judged tool trajectory no longer serves the objective".into()],
                turn: 3,
                round: 1,
            },
            "assessment-event-1",
            "2026-07-30 12:00:00.000000",
        )
        .expect("valid assessed projection"))
    }
}

// ─── Harness ────────────────────────────────────────────────────────────────

fn build_test_app() -> Router {
    let state = AppState::new(ServiceInfo::default(), Arc::new(StubHealth))
        .with_auth_service(Arc::new(StubAuth))
        .with_introspection_service(Arc::new(StubIntrospection));

    Router::new()
        .route(
            "/introspection/decision-trace",
            get(get_decision_trace_handler),
        )
        .route("/introspection/tool-history", get(get_tool_history_handler))
        .route("/introspection/drift-check", get(get_drift_check_handler))
        .with_state(state)
}

async fn get_json(app: Router, path: &str) -> (StatusCode, Value) {
    let req = Request::builder()
        .uri(path)
        .header("Authorization", "Bearer test-token")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let value: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, value)
}

// ─── decision-trace ─────────────────────────────────────────────────────────

#[tokio::test]
async fn decision_trace_returns_versioned_schema() {
    let app = build_test_app();
    let (status, body) =
        get_json(app, "/introspection/decision-trace?session_id=s1&last_n=20").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["schema_version"], 1);
    assert_eq!(body["session_id"], "s1");
    assert_eq!(body["last_n"], 20);
    assert!(body["decisions"].is_array());
}

#[tokio::test]
async fn decision_trace_default_last_n_is_20() {
    let app = build_test_app();
    let (status, body) = get_json(app, "/introspection/decision-trace?session_id=s1").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["last_n"], 20);
}

#[tokio::test]
async fn decision_trace_requires_session_id() {
    let app = build_test_app();
    let (status, _) = get_json(app, "/introspection/decision-trace").await;
    assert!(status.is_client_error(), "missing session_id must 4xx");
}

// ─── tool-history ───────────────────────────────────────────────────────────

#[tokio::test]
async fn tool_history_returns_versioned_schema() {
    let app = build_test_app();
    let (status, body) =
        get_json(app, "/introspection/tool-history?tool=grep&window_hours=24").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["schema_version"], 1);
    assert_eq!(body["tool"], "grep");
    assert_eq!(body["window_hours"], 24);
    assert!(body["total_calls"].is_number());
    assert!(body["success_rate"].is_number());
    assert!(body["recent_failures"].is_array());
}

#[tokio::test]
async fn tool_history_default_window_is_24() {
    let app = build_test_app();
    let (status, body) = get_json(app, "/introspection/tool-history?tool=grep").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["window_hours"], 24);
}

#[tokio::test]
async fn tool_history_requires_tool_name() {
    let app = build_test_app();
    let (status, _) = get_json(app, "/introspection/tool-history").await;
    assert!(status.is_client_error());
}

// ─── drift-check ────────────────────────────────────────────────────────────

#[tokio::test]
async fn drift_check_returns_versioned_schema() {
    let app = build_test_app();
    let (status, body) = get_json(app, "/introspection/drift-check?session_id=s1").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["schema_version"], 2);
    assert_eq!(body["session_id"], "s1");
    assert_eq!(body["assessment_status"], "assessed");
    assert_eq!(body["verdict"], "drifting");
    assert!(body["score"].is_number());
    let drift = body["score"].as_f64().unwrap();
    assert!(
        (0.0..=1.0).contains(&drift),
        "assessment score must be in [0,1], got {drift}"
    );
    assert_eq!(body["level"], "moderate");
    assert!(body["evidence"].is_array());
    assert_eq!(body["provenance"]["kind"], "llm_judge");
    assert!(body.get("original_intent_preview").is_none());
    assert!(body.get("current_focus_preview").is_none());
}

#[tokio::test]
async fn drift_check_requires_session_id() {
    let app = build_test_app();
    let (status, _) = get_json(app, "/introspection/drift-check").await;
    assert!(status.is_client_error());
}

// ─── Auth gating ─────────────────────────────────────────────────────────────

#[tokio::test]
async fn endpoints_reject_missing_auth_header() {
    let app = build_test_app();
    let req = Request::builder()
        .uri("/introspection/decision-trace?session_id=s1")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert!(
        resp.status() == StatusCode::UNAUTHORIZED || resp.status() == StatusCode::FORBIDDEN,
        "missing auth header must 401/403, got {}",
        resp.status()
    );
}
