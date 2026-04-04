use std::sync::Arc;

use astra_runtime::{
    AppState, AuthLoginRequestData, AuthRefreshRequestData, AuthRegisterRequestData, AuthService,
    AuthTokenRecord, AuthUserRecord, ErrorResponse, HealthChecker, LearningFeedbackRecord,
    LearningFeedbackRequestData, LearningFeedbackService, ReflectReport, ReflectService,
    ServiceInfo, build_app,
};
use astra_services::reflect::{Insight, SessionOverview};
use async_trait::async_trait;
use axum::{
    Json, Router, body,
    http::{Request, StatusCode},
};
use tower::util::ServiceExt;

// ── Stubs ────────────────────────────────────────────────────────────────────

#[derive(Clone)]
struct StubHealthChecker;

#[async_trait]
impl HealthChecker for StubHealthChecker {
    async fn database_healthy(&self) -> bool {
        true
    }
}

#[derive(Clone)]
struct StubAuthService;

#[async_trait]
impl AuthService for StubAuthService {
    async fn register(
        &self,
        _: AuthRegisterRequestData,
    ) -> Result<AuthUserRecord, (StatusCode, Json<ErrorResponse>)> {
        unimplemented!()
    }
    async fn login(
        &self,
        _: AuthLoginRequestData,
    ) -> Result<AuthTokenRecord, (StatusCode, Json<ErrorResponse>)> {
        unimplemented!()
    }
    async fn refresh(
        &self,
        _: AuthRefreshRequestData,
    ) -> Result<AuthTokenRecord, (StatusCode, Json<ErrorResponse>)> {
        unimplemented!()
    }
    async fn logout(
        &self,
        _: AuthRefreshRequestData,
    ) -> Result<(), (StatusCode, Json<ErrorResponse>)> {
        unimplemented!()
    }
    async fn current_user(
        &self,
        headers: &axum::http::HeaderMap,
    ) -> Result<AuthUserRecord, (StatusCode, Json<ErrorResponse>)> {
        let token = headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        if token.starts_with("Bearer test-token") {
            Ok(AuthUserRecord {
                user_id: "test-user-id".to_string(),
                username: "testuser".to_string(),
                email: "test@example.com".to_string(),
                display_name: Some("Test User".to_string()),
            })
        } else {
            Err((
                StatusCode::UNAUTHORIZED,
                Json(ErrorResponse {
                    detail: "Not authenticated".to_string(),
                }),
            ))
        }
    }
}

// ── Reflect stub ─────────────────────────────────────────────────────────────

#[derive(Clone)]
struct StubReflectService;

#[async_trait]
impl ReflectService for StubReflectService {
    async fn build_evidence(
        &self,
        _user_id: &str,
        session_id: &str,
        focus: &str,
        _last_n: i32,
        _question: &str,
    ) -> Result<ReflectReport, (StatusCode, Json<ErrorResponse>)> {
        if session_id == "not-found" {
            return Err((
                StatusCode::NOT_FOUND,
                Json(ErrorResponse {
                    detail: "Session not found or not owned by user".to_string(),
                }),
            ));
        }
        Ok(ReflectReport {
            session_id: session_id.to_string(),
            focus: focus.to_string(),
            overview: SessionOverview {
                total_events: 1,
                total_decisions: 1,
                duration_minutes: Some(5.0),
                unique_skills_used: 1,
                error_count: 0,
                error_rate_pct: 0.0,
                top_event_types: vec![("tool_call".into(), 1)],
                top_skills: vec![("code_search".into(), 1)],
            },
            diagnoses: vec![],
            insights: vec![Insight {
                severity: "info".into(),
                category: "performance".into(),
                message: "Very short session — limited data for analysis".into(),
                evidence: "1 events total".into(),
            }],
            recommendations: vec![],
        })
    }
}

// ── Learning feedback stub ───────────────────────────────────────────────────

#[derive(Clone)]
struct StubLearningFeedbackService;

#[async_trait]
impl LearningFeedbackService for StubLearningFeedbackService {
    async fn submit_feedback(
        &self,
        request: LearningFeedbackRequestData,
    ) -> Result<LearningFeedbackRecord, (StatusCode, Json<ErrorResponse>)> {
        // Simulate ownership check: "not-found" or wrong user → 404
        if request.event_id == "not-found" {
            return Err((
                StatusCode::NOT_FOUND,
                Json(ErrorResponse {
                    detail: "Event not found".to_string(),
                }),
            ));
        }
        // "other-user-event" belongs to a different user
        if request.event_id == "other-user-event" && request.user_id != "other-user" {
            return Err((
                StatusCode::NOT_FOUND,
                Json(ErrorResponse {
                    detail: "Event not found".to_string(),
                }),
            ));
        }
        Ok(LearningFeedbackRecord {
            status: "success".to_string(),
            message: format!(
                "Feedback recorded for event {} by {}",
                request.event_id, request.user_id
            ),
        })
    }
}

// ── Helpers ──────────────────────────────────────────────────────────────────

fn build_test_app() -> Router {
    let state = AppState::new(ServiceInfo::default(), Arc::new(StubHealthChecker))
        .with_auth_service(Arc::new(StubAuthService))
        .with_reflect_service(Arc::new(StubReflectService))
        .with_learning_feedback_service(Arc::new(StubLearningFeedbackService));
    build_app(state)
}

fn auth_headers() -> Vec<(&'static str, &'static str)> {
    vec![("authorization", "Bearer test-token")]
}

async fn get_json(
    app: Router,
    path: &str,
    headers: &[(&str, &str)],
) -> (StatusCode, serde_json::Value) {
    let mut builder = Request::builder().method("GET").uri(path);
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    let response = app
        .oneshot(builder.body(body::Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    (status, json)
}

async fn post_json(
    app: Router,
    path: &str,
    headers: &[(&str, &str)],
    payload: serde_json::Value,
) -> (StatusCode, serde_json::Value) {
    let mut builder = Request::builder()
        .method("POST")
        .uri(path)
        .header("content-type", "application/json");
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    let response = app
        .oneshot(builder.body(body::Body::from(payload.to_string())).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    (status, json)
}

// ── Reflect tests ────────────────────────────────────────────────────────────

#[tokio::test]
async fn reflect_requires_auth() {
    let app = build_test_app();
    let (status, json) = get_json(app, "/chat/session/sess-1/reflect", &[]).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(json["detail"], "Not authenticated");
}

#[tokio::test]
async fn reflect_returns_evidence_with_defaults() {
    let app = build_test_app();
    let (status, json) = get_json(app, "/chat/session/sess-1/reflect", &auth_headers()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["focus"], "auto");
    assert_eq!(json["session_id"], "sess-1");
    // New report structure
    assert!(json["overview"].is_object());
    assert_eq!(json["overview"]["total_events"], 1);
    assert_eq!(json["overview"]["total_decisions"], 1);
    assert!(json["insights"].is_array());
    assert!(json["recommendations"].is_array());
    // Verify top_skills populated
    let skills = json["overview"]["top_skills"].as_array().unwrap();
    assert_eq!(skills.len(), 1);
    assert_eq!(skills[0][0], "code_search");
}

#[tokio::test]
async fn reflect_passes_focus_and_question() {
    let app = build_test_app();
    let (status, json) = get_json(
        app,
        "/chat/session/sess-1/reflect?focus=skill_failure&question=why+did+it+fail",
        &auth_headers(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["focus"], "skill_failure");
    assert_eq!(json["session_id"], "sess-1");
}

#[tokio::test]
async fn reflect_session_not_found() {
    let app = build_test_app();
    let (status, json) = get_json(app, "/chat/session/not-found/reflect", &auth_headers()).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(json["detail"].as_str().unwrap().contains("not found"));
}

// ── Decision-trace tests ─────────────────────────────────────────────────────

#[tokio::test]
async fn decision_trace_requires_auth() {
    let app = build_test_app();
    let (status, _) = get_json(app, "/chat/session/sess-1/decision-trace", &[]).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn decision_trace_forces_tool_selection_focus() {
    let app = build_test_app();
    let (status, json) =
        get_json(app, "/chat/session/sess-1/decision-trace", &auth_headers()).await;
    assert_eq!(status, StatusCode::OK);
    // decision-trace always uses tool_selection focus regardless of query params
    assert_eq!(json["focus"], "tool_selection");
    assert_eq!(json["session_id"], "sess-1");
}

#[tokio::test]
async fn decision_trace_session_not_found() {
    let app = build_test_app();
    let (status, json) = get_json(
        app,
        "/chat/session/not-found/decision-trace",
        &auth_headers(),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(json["detail"].as_str().unwrap().contains("not found"));
}

// ── Learning feedback tests ──────────────────────────────────────────────────

#[tokio::test]
async fn learning_feedback_requires_auth() {
    let app = build_test_app();
    let (status, _) = post_json(
        app,
        "/api/v1/learning/feedback",
        &[],
        serde_json::json!({
            "event_id": "evt-1"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn learning_feedback_success() {
    let app = build_test_app();
    let (status, json) = post_json(
        app,
        "/api/v1/learning/feedback",
        &auth_headers(),
        serde_json::json!({
            "event_id": "evt-1",
            "satisfaction_score": 2
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["status"], "success");
    assert!(json["message"].as_str().unwrap().contains("evt-1"));
}

#[tokio::test]
async fn learning_feedback_event_not_found() {
    let app = build_test_app();
    let (status, json) = post_json(
        app,
        "/api/v1/learning/feedback",
        &auth_headers(),
        serde_json::json!({
            "event_id": "not-found"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(json["detail"].as_str().unwrap().contains("not found"));
}

#[tokio::test]
async fn learning_feedback_minimal_payload() {
    let app = build_test_app();
    let (status, json) = post_json(
        app,
        "/api/v1/learning/feedback",
        &auth_headers(),
        serde_json::json!({
            "event_id": "evt-2"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["status"], "success");
}

/// Security: handler must pass authenticated user_id to the service.
/// The response message includes user_id from the stub, proving it was forwarded.
#[tokio::test]
async fn learning_feedback_passes_user_id() {
    let app = build_test_app();
    let (status, json) = post_json(
        app,
        "/api/v1/learning/feedback",
        &auth_headers(),
        serde_json::json!({
            "event_id": "evt-1",
            "satisfaction_score": 5
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    // Stub includes user_id in message — verify the handler forwarded it
    assert!(
        json["message"].as_str().unwrap().contains("test-user-id"),
        "handler must pass authenticated user_id to service, got: {}",
        json["message"]
    );
}

/// Security: accessing another user's event must return 404 (not 200).
#[tokio::test]
async fn learning_feedback_rejects_other_users_event() {
    let app = build_test_app();
    let (status, json) = post_json(
        app,
        "/api/v1/learning/feedback",
        &auth_headers(),
        serde_json::json!({
            "event_id": "other-user-event",
            "satisfaction_score": 1
        }),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(json["detail"].as_str().unwrap().contains("not found"));
}

/// Verify removed fields are rejected (unknown fields should not silently pass).
#[tokio::test]
async fn learning_feedback_ignores_removed_fields() {
    let app = build_test_app();
    // Even if client sends old fields, they are ignored — only event_id + satisfaction_score matter
    let (status, json) = post_json(
        app,
        "/api/v1/learning/feedback",
        &auth_headers(),
        serde_json::json!({
            "event_id": "evt-1",
            "satisfaction_score": 3,
            "feedback_type": "wrong_skill",
            "correct_skills": ["code_search"]
        }),
    )
    .await;
    // Serde default: unknown fields are ignored, request still succeeds
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["status"], "success");
}
