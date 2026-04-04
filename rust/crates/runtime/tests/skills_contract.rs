use std::sync::Arc;

use astra_runtime::skills::{
    SkillInfoRecord, SkillListRecord, SkillPublishRequestData, SkillRegisterRequestData,
    SkillStatusRecord, SkillVersionRecord,
};
use astra_runtime::{
    AppState, AuthLoginRequestData, AuthRefreshRequestData, AuthRegisterRequestData, AuthService,
    AuthTokenRecord, AuthUserRecord, ErrorResponse, HealthChecker, ServiceInfo, SkillRecord,
    SkillService, build_app,
};
use async_trait::async_trait;
use axum::{
    body,
    http::{HeaderMap, Request, StatusCode},
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
    ) -> Result<AuthUserRecord, (StatusCode, axum::Json<ErrorResponse>)> {
        unreachable!()
    }
    async fn login(
        &self,
        _: AuthLoginRequestData,
    ) -> Result<AuthTokenRecord, (StatusCode, axum::Json<ErrorResponse>)> {
        unreachable!()
    }
    async fn refresh(
        &self,
        _: AuthRefreshRequestData,
    ) -> Result<AuthTokenRecord, (StatusCode, axum::Json<ErrorResponse>)> {
        unreachable!()
    }
    async fn logout(
        &self,
        _: AuthRefreshRequestData,
    ) -> Result<(), (StatusCode, axum::Json<ErrorResponse>)> {
        unreachable!()
    }

    async fn current_user(
        &self,
        headers: &HeaderMap,
    ) -> Result<AuthUserRecord, (StatusCode, axum::Json<ErrorResponse>)> {
        match headers.get("authorization").and_then(|v| v.to_str().ok()) {
            Some("Bearer test-token") => Ok(AuthUserRecord {
                user_id: "skill-user-id".to_string(),
                username: "skill-user".to_string(),
                email: "skill-user@test.com".to_string(),
                display_name: None,
            }),
            _ => Err((
                StatusCode::UNAUTHORIZED,
                axum::Json(ErrorResponse {
                    detail: "Not authenticated".to_string(),
                }),
            )),
        }
    }
}

// ── InMemory SkillService ────────────────────────────────────────────────────

#[derive(Clone)]
struct InMemorySkillService;

#[async_trait]
impl SkillService for InMemorySkillService {
    async fn register_skill(
        &self,
        _user_id: String,
        request: SkillRegisterRequestData,
    ) -> Result<SkillRecord, (StatusCode, axum::Json<ErrorResponse>)> {
        let skill_id = if request.skill_id.is_empty() {
            format!("{}@{}", request.skill_name, request.skill_version)
        } else {
            request.skill_id
        };
        Ok(SkillRecord {
            skill_id,
            skill_name: request.skill_name,
            version: request.skill_version,
            description: request.description,
            metadata: request.metadata,
            created_at: Some("2026-01-01T00:00:00".to_string()),
        })
    }

    async fn list_skills(
        &self,
        limit: u32,
        offset: u32,
    ) -> Result<SkillListRecord, (StatusCode, axum::Json<ErrorResponse>)> {
        Ok(SkillListRecord {
            skills: vec![serde_json::json!({
                "skill_id": "hello@1.0.0",
                "skill_name": "hello",
                "version": "1.0.0",
                "description": "A greeting skill",
            })],
            total: 1,
            limit,
            offset,
        })
    }

    async fn get_skill(
        &self,
        skill_id: String,
        _version: Option<String>,
    ) -> Result<SkillRecord, (StatusCode, axum::Json<ErrorResponse>)> {
        if skill_id == "hello@1.0.0" || skill_id == "hello" {
            Ok(SkillRecord {
                skill_id: "hello@1.0.0".to_string(),
                skill_name: "hello".to_string(),
                version: "1.0.0".to_string(),
                description: Some("A greeting skill".to_string()),
                metadata: None,
                created_at: Some("2026-01-01T00:00:00".to_string()),
            })
        } else {
            Err((
                StatusCode::NOT_FOUND,
                axum::Json(ErrorResponse {
                    detail: format!("Skill '{}' not found", skill_id),
                }),
            ))
        }
    }

    async fn get_skill_info(
        &self,
        skill_name: String,
        _user_id: String,
    ) -> Result<SkillInfoRecord, (StatusCode, axum::Json<ErrorResponse>)> {
        if skill_name == "hello" {
            Ok(SkillInfoRecord {
                skill_name: "hello".to_string(),
                version: "1.0.0".to_string(),
                description: Some("A greeting skill".to_string()),
                source: Some("user".to_string()),
                status: Some("active".to_string()),
                created_by: Some("skill-user-id".to_string()),
                category: Some("general".to_string()),
                install_count: 5,
                created_at: Some("2026-01-01T00:00:00".to_string()),
            })
        } else {
            Err((
                StatusCode::NOT_FOUND,
                axum::Json(ErrorResponse {
                    detail: format!("Skill '{}' not found", skill_name),
                }),
            ))
        }
    }

    async fn list_skill_versions(
        &self,
        _skill_name: String,
    ) -> Result<Vec<SkillVersionRecord>, (StatusCode, axum::Json<ErrorResponse>)> {
        Ok(vec![
            SkillVersionRecord {
                version: "1.0.0".to_string(),
                status: Some("active".to_string()),
                is_active: Some(1),
                created_at: Some("2026-01-01T00:00:00".to_string()),
            },
            SkillVersionRecord {
                version: "0.9.0".to_string(),
                status: Some("inactive".to_string()),
                is_active: Some(0),
                created_at: Some("2025-12-01T00:00:00".to_string()),
            },
        ])
    }

    async fn get_skill_status(
        &self,
        _user_id: String,
        _per_group: u32,
    ) -> Result<SkillStatusRecord, (StatusCode, axum::Json<ErrorResponse>)> {
        Ok(SkillStatusRecord {
            builtin: vec![serde_json::json!({"skill_name": "builtin-skill"})],
            marketplace: vec![serde_json::json!({"skill_name": "mp-skill"})],
            user: vec![serde_json::json!({"skill_name": "user-skill"})],
            platform_total: 2,
            user_total: 1,
        })
    }

    async fn publish_skill(
        &self,
        _user_id: String,
        request: SkillPublishRequestData,
    ) -> Result<serde_json::Value, (StatusCode, axum::Json<ErrorResponse>)> {
        Ok(serde_json::json!({
            "skill_id": format!("{}@{}", request.name, request.version),
            "skill_name": request.name,
            "version": request.version,
            "status": "published",
        }))
    }

    async fn unpublish_skill(
        &self,
        _user_id: String,
        skill_name: String,
    ) -> Result<serde_json::Value, (StatusCode, axum::Json<ErrorResponse>)> {
        Ok(serde_json::json!({"skill_name": skill_name, "result": "unpublished"}))
    }
}

fn build_test_app() -> axum::Router {
    let state = AppState::new(ServiceInfo::default(), Arc::new(StubHealthChecker))
        .with_auth_service(Arc::new(StubAuthService))
        .with_skill_service(Arc::new(InMemorySkillService));
    build_app(state)
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn register_skill_returns_created() {
    let app = build_test_app();
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/skills")
                .header("authorization", "Bearer test-token")
                .header("content-type", "application/json")
                .body(body::Body::from(
                    r#"{"skill_id":"hello@1.0.0","skill_name":"hello","skill_version":"1.0.0","skill_code":"print('hi')","description":"A greeting skill"}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let bytes = body::to_bytes(resp.into_body(), 1024 * 64).await.unwrap();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(json["skill_id"], "hello@1.0.0");
    assert_eq!(json["skill_name"], "hello");
    assert_eq!(json["version"], "1.0.0");
    assert_eq!(json["description"], "A greeting skill");
}

#[tokio::test]
async fn list_skills_returns_ok() {
    let app = build_test_app();
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/skills")
                .header("authorization", "Bearer test-token")
                .body(body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = body::to_bytes(resp.into_body(), 1024 * 64).await.unwrap();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(json["total"], 1);
    assert_eq!(json["skills"][0]["skill_name"], "hello");
}

#[tokio::test]
async fn list_skills_with_query_params() {
    let app = build_test_app();
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/skills?limit=10&offset=0")
                .header("authorization", "Bearer test-token")
                .body(body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = body::to_bytes(resp.into_body(), 1024 * 64).await.unwrap();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(json["limit"], 10);
    assert_eq!(json["offset"], 0);
}

#[tokio::test]
async fn get_skill_returns_ok() {
    let app = build_test_app();
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/skills/hello@1.0.0")
                .header("authorization", "Bearer test-token")
                .body(body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = body::to_bytes(resp.into_body(), 1024 * 64).await.unwrap();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(json["skill_id"], "hello@1.0.0");
    assert_eq!(json["skill_name"], "hello");
}

#[tokio::test]
async fn get_skill_not_found() {
    let app = build_test_app();
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/skills/nonexistent")
                .header("authorization", "Bearer test-token")
                .body(body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn get_skill_info_returns_ok() {
    let app = build_test_app();
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/skills/hello/info")
                .header("authorization", "Bearer test-token")
                .body(body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = body::to_bytes(resp.into_body(), 1024 * 64).await.unwrap();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(json["skill_name"], "hello");
    assert_eq!(json["version"], "1.0.0");
    assert_eq!(json["install_count"], 5);
}

#[tokio::test]
async fn get_skill_info_not_found() {
    let app = build_test_app();
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/skills/nonexistent/info")
                .header("authorization", "Bearer test-token")
                .body(body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn list_skill_versions_returns_ok() {
    let app = build_test_app();
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/skills/hello@1.0.0/versions")
                .header("authorization", "Bearer test-token")
                .body(body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = body::to_bytes(resp.into_body(), 1024 * 64).await.unwrap();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let arr = json.as_array().unwrap();
    assert_eq!(arr.len(), 2);
    assert_eq!(arr[0]["version"], "1.0.0");
    assert_eq!(arr[1]["version"], "0.9.0");
}

#[tokio::test]
async fn get_skill_status_returns_ok() {
    let app = build_test_app();
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/skills/status")
                .header("authorization", "Bearer test-token")
                .body(body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = body::to_bytes(resp.into_body(), 1024 * 64).await.unwrap();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(json["platform_total"], 2);
    assert_eq!(json["user_total"], 1);
    assert!(json["builtin"].as_array().unwrap().len() == 1);
    assert!(json["marketplace"].as_array().unwrap().len() == 1);
    assert!(json["user"].as_array().unwrap().len() == 1);
}

#[tokio::test]
async fn publish_skill_returns_created() {
    let app = build_test_app();
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/skills/publish")
                .header("authorization", "Bearer test-token")
                .header("content-type", "application/json")
                .body(body::Body::from(
                    r#"{"name":"my-skill","version":"1.0.0","description":"Published skill","category":"user","priority":5}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let bytes = body::to_bytes(resp.into_body(), 1024 * 64).await.unwrap();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(json["skill_id"], "my-skill@1.0.0");
    assert_eq!(json["status"], "published");
}

#[tokio::test]
async fn unpublish_skill_returns_ok() {
    let app = build_test_app();
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/skills/hello/unpublish")
                .header("authorization", "Bearer test-token")
                .body(body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = body::to_bytes(resp.into_body(), 1024 * 64).await.unwrap();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(json["skill_name"], "hello");
    assert_eq!(json["result"], "unpublished");
}
