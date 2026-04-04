use std::{fs, path::PathBuf, sync::Arc};

use astra_runtime::{
    AdminAuditFilter, AdminAuditReader, AdminAuditRecord, AdminAuthorizer,
    AdminFeedbackStatsFilter, AdminFeedbackStatsReader, AdminFeedbackStatsRecord, AdminInitRecord,
    AdminInitializer, AdminTokenCreateRequestData, AdminTokenFilter, AdminTokenReader,
    AdminTokenRecord, AdminTokenWriter, AdminUserRoleManager, AdminUserRoleRecord,
    AdminUserRoleRequestData, AppState, AuthenticatedUser, ErrorResponse, HealthChecker,
    ServiceInfo, build_app,
};
use async_trait::async_trait;
use axum::{
    Router, body,
    http::{Request, StatusCode},
};
use serde::Deserialize;
use tower::util::ServiceExt;
use uuid::Uuid;

#[derive(Deserialize)]
struct ResponseContract {
    status: u16,
    json: serde_json::Value,
}

#[derive(Deserialize)]
struct AdminContract {
    auth_error: ResponseContract,
    admin_forbidden: ResponseContract,
    admin_init: ResponseContract,
    admin_token_create: CreateTokenContract,
    admin_prompt_optimize: QueueContract,
    admin_feedback_export: QueueContract,
    admin_feedback_stats: ResponseContract,
    admin_feedback_stats_filtered: ResponseContract,
    admin_role_grant: QueueContract,
    admin_role_grant_existing: QueueContract,
    admin_role_grant_user_not_found: QueueContract,
    admin_role_grant_role_not_found: QueueContract,
    admin_role_revoke: QueueContract,
    admin_role_revoke_missing: QueueContract,
    admin_role_revoke_user_not_found: QueueContract,
    admin_role_revoke_role_not_found: QueueContract,
    admin_tokens: ResponseContract,
    admin_tokens_llm_global: ResponseContract,
    admin_audit: ResponseContract,
    admin_audit_user_filtered: ResponseContract,
}

#[derive(Deserialize)]
struct CreateTokenContract {
    request: serde_json::Value,
    status: u16,
    json: serde_json::Value,
}

#[derive(Deserialize)]
struct QueueContract {
    request: serde_json::Value,
    status: u16,
    json: serde_json::Value,
}

#[derive(Clone)]
struct StubHealthChecker;

#[async_trait]
impl HealthChecker for StubHealthChecker {
    async fn database_healthy(&self) -> bool {
        true
    }
}

#[derive(Clone)]
struct StubAdminAuthorizer;

#[async_trait]
impl AdminAuthorizer for StubAdminAuthorizer {
    async fn require_admin(
        &self,
        headers: &axum::http::HeaderMap,
    ) -> Result<AuthenticatedUser, (StatusCode, axum::Json<ErrorResponse>)> {
        match headers
            .get("authorization")
            .and_then(|value| value.to_str().ok())
        {
            Some("Bearer admin-token") => Ok(AuthenticatedUser {
                user_id: "admin-1".to_string(),
                username: Some("admin".to_string()),
            }),
            Some("Bearer user-token") => Err((
                StatusCode::FORBIDDEN,
                axum::Json(ErrorResponse {
                    detail: "Admin role required".to_string(),
                }),
            )),
            _ => Err((
                StatusCode::UNAUTHORIZED,
                axum::Json(ErrorResponse {
                    detail: "Not authenticated".to_string(),
                }),
            )),
        }
    }
}

#[derive(Clone)]
struct StubAdminTokenReader;

#[async_trait]
impl AdminTokenReader for StubAdminTokenReader {
    async fn list_tokens(
        &self,
        filter: AdminTokenFilter,
    ) -> Result<Vec<AdminTokenRecord>, (StatusCode, axum::Json<ErrorResponse>)> {
        let mut tokens = vec![
            AdminTokenRecord {
                token_id: "contract-user-token".to_string(),
                token_type: "api".to_string(),
                provider: Some("github".to_string()),
                scope: "user".to_string(),
                scope_id: Some("contract-user-123".to_string()),
                created_at: "2026-01-02T09:30:00".to_string(),
            },
            AdminTokenRecord {
                token_id: "contract-global-token".to_string(),
                token_type: "llm".to_string(),
                provider: Some("openai".to_string()),
                scope: "global".to_string(),
                scope_id: None,
                created_at: "2026-01-01T12:00:00".to_string(),
            },
        ];

        if let Some(token_type) = filter.token_type {
            tokens.retain(|token| token.token_type == token_type);
        }
        if let Some(scope) = filter.scope {
            match scope.as_str() {
                "user" | "repo" | "global" => tokens.retain(|token| token.scope == scope),
                _ => {}
            }
        }

        Ok(tokens)
    }
}

#[derive(Clone)]
struct StubAdminInitializer;

#[async_trait]
impl AdminInitializer for StubAdminInitializer {
    async fn initialize(&self) -> Result<AdminInitRecord, (StatusCode, axum::Json<ErrorResponse>)> {
        Ok(AdminInitRecord {
            message: "Database initialized successfully".to_string(),
            tables_created: 0,
        })
    }
}

#[derive(Clone)]
struct StubAdminTokenWriter;

#[async_trait]
impl AdminTokenWriter for StubAdminTokenWriter {
    async fn create_token(
        &self,
        request: AdminTokenCreateRequestData,
    ) -> Result<AdminTokenRecord, (StatusCode, axum::Json<ErrorResponse>)> {
        Ok(AdminTokenRecord {
            token_id: "contract-created-token".to_string(),
            token_type: request.token_type,
            provider: request.provider.or(Some("unknown".to_string())),
            scope: request.scope,
            scope_id: request.scope_id,
            created_at: "2026-01-04T14:00:00".to_string(),
        })
    }
}

#[derive(Clone)]
struct StubAdminAuditReader;

#[async_trait]
impl AdminAuditReader for StubAdminAuditReader {
    async fn list_audit_logs(
        &self,
        filter: AdminAuditFilter,
    ) -> Result<Vec<AdminAuditRecord>, (StatusCode, axum::Json<ErrorResponse>)> {
        let mut logs = vec![
            AdminAuditRecord {
                log_id: "contract-log-2".to_string(),
                user_id: "contract-audit-user".to_string(),
                action: "revoke_role".to_string(),
                resource_type: "role".to_string(),
                resource_id: Some("mo_agent_admin".to_string()),
                timestamp: "2026-01-03T10:00:00".to_string(),
                details: Some(serde_json::json!({"username": "alice"})),
            },
            AdminAuditRecord {
                log_id: "contract-log-1".to_string(),
                user_id: "contract-audit-user".to_string(),
                action: "create_token".to_string(),
                resource_type: "token".to_string(),
                resource_id: Some("llm_openai".to_string()),
                timestamp: "2026-01-02T08:00:00".to_string(),
                details: Some(serde_json::json!({"scope": "global"})),
            },
        ];

        if let Some(user_id) = filter.user_id {
            logs.retain(|log| log.user_id == user_id);
        }
        if let Some(since) = filter.since {
            logs.retain(|log| log.timestamp >= since);
        }
        logs.truncate(filter.limit as usize);
        Ok(logs)
    }
}

#[derive(Clone)]
struct StubAdminFeedbackStatsReader;

#[async_trait]
impl AdminFeedbackStatsReader for StubAdminFeedbackStatsReader {
    async fn read_feedback_stats(
        &self,
        filter: AdminFeedbackStatsFilter,
    ) -> Result<AdminFeedbackStatsRecord, (StatusCode, axum::Json<ErrorResponse>)> {
        let all = AdminFeedbackStatsRecord {
            total_feedback: 3,
            positive_feedback: 1,
            negative_feedback: 1,
            avg_rating: Some(3.0),
            feedback_by_type: serde_json::Map::from_iter([
                ("wrong_skill".to_string(), serde_json::Value::from(2)),
                ("low_satisfaction".to_string(), serde_json::Value::from(1)),
            ]),
        };

        let filtered = AdminFeedbackStatsRecord {
            total_feedback: 2,
            positive_feedback: 1,
            negative_feedback: 1,
            avg_rating: Some(3.0),
            feedback_by_type: serde_json::Map::from_iter([
                ("wrong_skill".to_string(), serde_json::Value::from(1)),
                ("low_satisfaction".to_string(), serde_json::Value::from(1)),
            ]),
        };

        if filter.agent_id.as_deref() == Some("contract-agent")
            && filter.since.as_deref() == Some("2026-01-04 00:00:00")
        {
            Ok(filtered)
        } else {
            Ok(all)
        }
    }
}

#[derive(Clone)]
struct StubAdminUserRoleManager;

#[async_trait]
impl AdminUserRoleManager for StubAdminUserRoleManager {
    async fn grant_role(
        &self,
        request: AdminUserRoleRequestData,
    ) -> Result<AdminUserRoleRecord, (StatusCode, axum::Json<ErrorResponse>)> {
        if request.username == "contract-missing-user" {
            return Err((
                StatusCode::NOT_FOUND,
                axum::Json(ErrorResponse {
                    detail: "User not found".to_string(),
                }),
            ));
        }
        if request.role_name == "contract_missing_role" {
            return Err((
                StatusCode::NOT_FOUND,
                axum::Json(ErrorResponse {
                    detail: "Role not found".to_string(),
                }),
            ));
        }

        Ok(AdminUserRoleRecord {
            username: request.username.clone(),
            role_name: request.role_name.clone(),
            message: if request.username == "contract-existing-user" {
                "User already has this role".to_string()
            } else {
                "Role granted successfully".to_string()
            },
        })
    }

    async fn revoke_role(
        &self,
        request: AdminUserRoleRequestData,
    ) -> Result<AdminUserRoleRecord, (StatusCode, axum::Json<ErrorResponse>)> {
        if request.username == "contract-missing-user" {
            return Err((
                StatusCode::NOT_FOUND,
                axum::Json(ErrorResponse {
                    detail: "User not found".to_string(),
                }),
            ));
        }
        if request.role_name == "contract_missing_role" {
            return Err((
                StatusCode::NOT_FOUND,
                axum::Json(ErrorResponse {
                    detail: "Role not found".to_string(),
                }),
            ));
        }

        Ok(AdminUserRoleRecord {
            username: request.username.clone(),
            role_name: request.role_name.clone(),
            message: if request.username == "contract-without-role-user" {
                "User does not have this role".to_string()
            } else {
                "Role revoked successfully".to_string()
            },
        })
    }
}

fn load_contract() -> AdminContract {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../..")
        .join("fixtures/contracts/admin_contract.json");
    let content = fs::read_to_string(path).expect("admin contract fixture should exist");
    serde_json::from_str(&content).expect("admin contract fixture should be valid JSON")
}

fn build_app_with_admin() -> Router {
    build_app(
        AppState::new(ServiceInfo::default(), Arc::new(StubHealthChecker))
            .with_admin_authorizer(Arc::new(StubAdminAuthorizer))
            .with_admin_initializer(Arc::new(StubAdminInitializer))
            .with_admin_token_writer(Arc::new(StubAdminTokenWriter))
            .with_admin_token_reader(Arc::new(StubAdminTokenReader))
            .with_admin_audit_reader(Arc::new(StubAdminAuditReader))
            .with_admin_feedback_stats_reader(Arc::new(StubAdminFeedbackStatsReader))
            .with_admin_user_role_manager(Arc::new(StubAdminUserRoleManager)),
    )
}

async fn read_json(
    app: Router,
    path: &str,
    headers: &[(&str, &str)],
) -> (StatusCode, serde_json::Value) {
    let response = app
        .oneshot(build_request("GET", path, headers))
        .await
        .unwrap();
    let status = response.status();
    let bytes = body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json = serde_json::from_slice(&bytes).unwrap();
    (status, json)
}

async fn post_json(
    app: Router,
    path: &str,
    headers: &[(&str, &str)],
    payload: serde_json::Value,
) -> (StatusCode, serde_json::Value) {
    let response = app
        .oneshot(build_request_with_json("POST", path, headers, payload))
        .await
        .unwrap();
    let status = response.status();
    let bytes = body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json = serde_json::from_slice(&bytes).unwrap();
    (status, json)
}

fn build_request(method: &str, path: &str, headers: &[(&str, &str)]) -> Request<body::Body> {
    let mut builder = Request::builder().method(method).uri(path);
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    builder.body(body::Body::empty()).unwrap()
}

fn build_request_with_json(
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    payload: serde_json::Value,
) -> Request<body::Body> {
    let mut builder = Request::builder()
        .method(method)
        .uri(path)
        .header("content-type", "application/json");
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    builder.body(body::Body::from(payload.to_string())).unwrap()
}

#[tokio::test]
async fn admin_tokens_require_auth() {
    let contract = load_contract();

    let (status, json) = read_json(build_app_with_admin(), "/admin/tokens", &[]).await;

    assert_eq!(status.as_u16(), contract.auth_error.status);
    assert_eq!(json, contract.auth_error.json);
}

#[tokio::test]
async fn admin_init_require_auth() {
    let contract = load_contract();

    let (status, json) = post_json(
        build_app_with_admin(),
        "/admin/init",
        &[],
        serde_json::json!({}),
    )
    .await;

    assert_eq!(status.as_u16(), contract.auth_error.status);
    assert_eq!(json, contract.auth_error.json);
}

#[tokio::test]
async fn admin_init_require_admin_role() {
    let contract = load_contract();

    let (status, json) = post_json(
        build_app_with_admin(),
        "/admin/init",
        &[("authorization", "Bearer user-token")],
        serde_json::json!({}),
    )
    .await;

    assert_eq!(status.as_u16(), contract.admin_forbidden.status);
    assert_eq!(json, contract.admin_forbidden.json);
}

#[tokio::test]
async fn admin_init_matches_shared_contract() {
    let contract = load_contract();

    let (status, json) = post_json(
        build_app_with_admin(),
        "/admin/init",
        &[("authorization", "Bearer admin-token")],
        serde_json::json!({}),
    )
    .await;

    assert_eq!(status.as_u16(), contract.admin_init.status);
    assert_eq!(json, contract.admin_init.json);
}

#[tokio::test]
async fn admin_tokens_require_admin_role() {
    let contract = load_contract();

    let (status, json) = read_json(
        build_app_with_admin(),
        "/admin/tokens",
        &[("authorization", "Bearer user-token")],
    )
    .await;

    assert_eq!(status.as_u16(), contract.admin_forbidden.status);
    assert_eq!(json, contract.admin_forbidden.json);
}

#[tokio::test]
async fn admin_token_create_requires_admin_role() {
    let contract = load_contract();

    let (status, json) = post_json(
        build_app_with_admin(),
        "/admin/tokens",
        &[("authorization", "Bearer user-token")],
        contract.admin_token_create.request.clone(),
    )
    .await;

    assert_eq!(status.as_u16(), contract.admin_forbidden.status);
    assert_eq!(json, contract.admin_forbidden.json);
}

#[tokio::test]
async fn admin_token_create_matches_shared_contract() {
    let contract = load_contract();

    let (status, json) = post_json(
        build_app_with_admin(),
        "/admin/tokens",
        &[("authorization", "Bearer admin-token")],
        contract.admin_token_create.request.clone(),
    )
    .await;

    assert_eq!(status.as_u16(), contract.admin_token_create.status);
    assert_eq!(
        json["token_type"],
        contract.admin_token_create.json["token_type"]
    );
    assert_eq!(
        json["provider"],
        contract.admin_token_create.json["provider"]
    );
    assert_eq!(json["scope"], contract.admin_token_create.json["scope"]);
    assert_eq!(
        json["scope_id"],
        contract.admin_token_create.json["scope_id"]
    );
    assert_eq!(
        json["token_id"],
        serde_json::Value::String("contract-created-token".into())
    );
    assert_eq!(
        json["created_at"],
        serde_json::Value::String("2026-01-04T14:00:00".into())
    );
}

#[tokio::test]
async fn admin_prompt_optimize_matches_shared_contract() {
    let contract = load_contract();

    let (status, json) = post_json(
        build_app_with_admin(),
        "/admin/prompts/optimize",
        &[("authorization", "Bearer admin-token")],
        contract.admin_prompt_optimize.request.clone(),
    )
    .await;

    assert_eq!(status.as_u16(), contract.admin_prompt_optimize.status);
    assert_eq!(
        json["status"],
        contract.admin_prompt_optimize.json["status"]
    );
    assert_eq!(
        json["message"],
        contract.admin_prompt_optimize.json["message"]
    );
    assert!(Uuid::parse_str(json["job_id"].as_str().unwrap()).is_ok());
}

#[tokio::test]
async fn admin_feedback_export_matches_shared_contract() {
    let contract = load_contract();

    let (status, json) = post_json(
        build_app_with_admin(),
        "/admin/feedback/export",
        &[("authorization", "Bearer admin-token")],
        contract.admin_feedback_export.request.clone(),
    )
    .await;

    assert_eq!(status.as_u16(), contract.admin_feedback_export.status);
    assert_eq!(
        json["status"],
        contract.admin_feedback_export.json["status"]
    );
    assert_eq!(
        json["download_url"],
        contract.admin_feedback_export.json["download_url"]
    );
    assert!(Uuid::parse_str(json["job_id"].as_str().unwrap()).is_ok());
}

#[tokio::test]
async fn admin_feedback_stats_matches_shared_contract() {
    let contract = load_contract();

    let (status, json) = read_json(
        build_app_with_admin(),
        "/admin/feedback/stats?agent_id=contract-agent",
        &[("authorization", "Bearer admin-token")],
    )
    .await;

    assert_eq!(status.as_u16(), contract.admin_feedback_stats.status);
    assert_eq!(json, contract.admin_feedback_stats.json);
}

#[tokio::test]
async fn admin_feedback_stats_filtered_matches_shared_contract() {
    let contract = load_contract();

    let (status, json) = read_json(
        build_app_with_admin(),
        "/admin/feedback/stats?agent_id=contract-agent&since=2026-01-04%2000:00:00",
        &[("authorization", "Bearer admin-token")],
    )
    .await;

    assert_eq!(
        status.as_u16(),
        contract.admin_feedback_stats_filtered.status
    );
    assert_eq!(json, contract.admin_feedback_stats_filtered.json);
}

#[tokio::test]
async fn admin_role_grant_matches_shared_contract() {
    let contract = load_contract();

    let (status, json) = post_json(
        build_app_with_admin(),
        "/admin/users/grant-role",
        &[("authorization", "Bearer admin-token")],
        contract.admin_role_grant.request.clone(),
    )
    .await;

    assert_eq!(status.as_u16(), contract.admin_role_grant.status);
    assert_eq!(json, contract.admin_role_grant.json);
}

#[tokio::test]
async fn admin_role_grant_existing_matches_shared_contract() {
    let contract = load_contract();

    let (status, json) = post_json(
        build_app_with_admin(),
        "/admin/users/grant-role",
        &[("authorization", "Bearer admin-token")],
        contract.admin_role_grant_existing.request.clone(),
    )
    .await;

    assert_eq!(status.as_u16(), contract.admin_role_grant_existing.status);
    assert_eq!(json, contract.admin_role_grant_existing.json);
}

#[tokio::test]
async fn admin_role_grant_user_not_found_matches_shared_contract() {
    let contract = load_contract();

    let (status, json) = post_json(
        build_app_with_admin(),
        "/admin/users/grant-role",
        &[("authorization", "Bearer admin-token")],
        contract.admin_role_grant_user_not_found.request.clone(),
    )
    .await;

    assert_eq!(
        status.as_u16(),
        contract.admin_role_grant_user_not_found.status
    );
    assert_eq!(json, contract.admin_role_grant_user_not_found.json);
}

#[tokio::test]
async fn admin_role_grant_role_not_found_matches_shared_contract() {
    let contract = load_contract();

    let (status, json) = post_json(
        build_app_with_admin(),
        "/admin/users/grant-role",
        &[("authorization", "Bearer admin-token")],
        contract.admin_role_grant_role_not_found.request.clone(),
    )
    .await;

    assert_eq!(
        status.as_u16(),
        contract.admin_role_grant_role_not_found.status
    );
    assert_eq!(json, contract.admin_role_grant_role_not_found.json);
}

#[tokio::test]
async fn admin_role_revoke_matches_shared_contract() {
    let contract = load_contract();

    let (status, json) = post_json(
        build_app_with_admin(),
        "/admin/users/revoke-role",
        &[("authorization", "Bearer admin-token")],
        contract.admin_role_revoke.request.clone(),
    )
    .await;

    assert_eq!(status.as_u16(), contract.admin_role_revoke.status);
    assert_eq!(json, contract.admin_role_revoke.json);
}

#[tokio::test]
async fn admin_role_revoke_missing_matches_shared_contract() {
    let contract = load_contract();

    let (status, json) = post_json(
        build_app_with_admin(),
        "/admin/users/revoke-role",
        &[("authorization", "Bearer admin-token")],
        contract.admin_role_revoke_missing.request.clone(),
    )
    .await;

    assert_eq!(status.as_u16(), contract.admin_role_revoke_missing.status);
    assert_eq!(json, contract.admin_role_revoke_missing.json);
}

#[tokio::test]
async fn admin_role_revoke_user_not_found_matches_shared_contract() {
    let contract = load_contract();

    let (status, json) = post_json(
        build_app_with_admin(),
        "/admin/users/revoke-role",
        &[("authorization", "Bearer admin-token")],
        contract.admin_role_revoke_user_not_found.request.clone(),
    )
    .await;

    assert_eq!(
        status.as_u16(),
        contract.admin_role_revoke_user_not_found.status
    );
    assert_eq!(json, contract.admin_role_revoke_user_not_found.json);
}

#[tokio::test]
async fn admin_role_revoke_role_not_found_matches_shared_contract() {
    let contract = load_contract();

    let (status, json) = post_json(
        build_app_with_admin(),
        "/admin/users/revoke-role",
        &[("authorization", "Bearer admin-token")],
        contract.admin_role_revoke_role_not_found.request.clone(),
    )
    .await;

    assert_eq!(
        status.as_u16(),
        contract.admin_role_revoke_role_not_found.status
    );
    assert_eq!(json, contract.admin_role_revoke_role_not_found.json);
}

#[tokio::test]
async fn admin_tokens_match_shared_contract() {
    let contract = load_contract();

    let (status, json) = read_json(
        build_app_with_admin(),
        "/admin/tokens",
        &[("authorization", "Bearer admin-token")],
    )
    .await;

    assert_eq!(status.as_u16(), contract.admin_tokens.status);
    assert_eq!(json, contract.admin_tokens.json);
}

#[tokio::test]
async fn admin_tokens_filters_match_shared_contract() {
    let contract = load_contract();

    let (status, json) = read_json(
        build_app_with_admin(),
        "/admin/tokens?token_type=llm&scope=global",
        &[("authorization", "Bearer admin-token")],
    )
    .await;

    assert_eq!(status.as_u16(), contract.admin_tokens_llm_global.status);
    assert_eq!(json, contract.admin_tokens_llm_global.json);
}

#[tokio::test]
async fn admin_audit_matches_shared_contract() {
    let contract = load_contract();

    let (status, json) = read_json(
        build_app_with_admin(),
        "/admin/audit",
        &[("authorization", "Bearer admin-token")],
    )
    .await;

    assert_eq!(status.as_u16(), contract.admin_audit.status);
    assert_eq!(json, contract.admin_audit.json);
}

#[tokio::test]
async fn admin_audit_filters_match_shared_contract() {
    let contract = load_contract();

    let (status, json) = read_json(
        build_app_with_admin(),
        "/admin/audit?user_id=contract-audit-user&limit=1",
        &[("authorization", "Bearer admin-token")],
    )
    .await;

    assert_eq!(status.as_u16(), contract.admin_audit_user_filtered.status);
    assert_eq!(json, contract.admin_audit_user_filtered.json);
}
