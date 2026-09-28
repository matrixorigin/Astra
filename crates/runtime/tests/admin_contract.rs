use std::{fs, sync::Arc};

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

mod test_support;
use test_support::assert_contract_json;

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
                axum::Json(ErrorResponse::new("Admin role required".to_string())),
            )),
            _ => Err((
                StatusCode::UNAUTHORIZED,
                axum::Json(ErrorResponse::new("Not authenticated".to_string())),
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
                resource_id: Some("astra_admin".to_string()),
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
    async fn has_role_members(
        &self,
        _role_name: &str,
    ) -> Result<bool, (StatusCode, axum::Json<ErrorResponse>)> {
        Ok(false)
    }

    async fn grant_role(
        &self,
        request: AdminUserRoleRequestData,
    ) -> Result<AdminUserRoleRecord, (StatusCode, axum::Json<ErrorResponse>)> {
        if request.username == "contract-missing-user" {
            return Err((
                StatusCode::NOT_FOUND,
                axum::Json(ErrorResponse::new("User not found".to_string())),
            ));
        }
        if request.role_name == "contract_missing_role" {
            return Err((
                StatusCode::NOT_FOUND,
                axum::Json(ErrorResponse::new("Role not found".to_string())),
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
                axum::Json(ErrorResponse::new("User not found".to_string())),
            ));
        }
        if request.role_name == "contract_missing_role" {
            return Err((
                StatusCode::NOT_FOUND,
                axum::Json(ErrorResponse::new("Role not found".to_string())),
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
    let content = fs::read_to_string(astra_core::test_paths::workspace_path(
        "fixtures/contracts/admin_contract.json",
    ))
    .expect("admin contract fixture should exist");
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
async fn admin_routes_require_auth() {
    let contract = load_contract();
    let app = build_app_with_admin();

    let (status, json) = read_json(app.clone(), "/admin/tokens", &[]).await;
    assert_eq!(status.as_u16(), contract.auth_error.status);
    assert_contract_json(&json, &contract.auth_error.json, "admin_tokens_auth_error");

    let (status, json) = post_json(app, "/admin/init", &[], serde_json::json!({})).await;
    assert_eq!(status.as_u16(), contract.auth_error.status);
    assert_contract_json(&json, &contract.auth_error.json, "admin_init_auth_error");
}

#[tokio::test]
async fn admin_init_variants_match_shared_contract() {
    let contract = load_contract();
    let app = build_app_with_admin();

    let (status, json) = post_json(
        app.clone(),
        "/admin/init",
        &[("authorization", "Bearer user-token")],
        serde_json::json!({}),
    )
    .await;
    assert_eq!(status.as_u16(), contract.admin_forbidden.status);
    assert_contract_json(
        &json,
        &contract.admin_forbidden.json,
        "admin_init_forbidden",
    );

    let (status, json) = post_json(
        app,
        "/admin/init",
        &[("authorization", "Bearer admin-token")],
        serde_json::json!({}),
    )
    .await;
    assert_eq!(status.as_u16(), contract.admin_init.status);
    assert_contract_json(&json, &contract.admin_init.json, "admin_init_success");
}

#[tokio::test]
async fn admin_routes_reject_non_admin_user_token() {
    let contract = load_contract();
    let app = build_app_with_admin();
    let user = &[("authorization", "Bearer user-token")];

    let (status, json) = read_json(app.clone(), "/admin/tokens", user).await;
    assert_eq!(status.as_u16(), contract.admin_forbidden.status);
    assert_contract_json(
        &json,
        &contract.admin_forbidden.json,
        "admin_tokens_non_admin_forbidden",
    );

    let (status, json) = post_json(
        app,
        "/admin/tokens",
        user,
        contract.admin_token_create.request.clone(),
    )
    .await;
    assert_eq!(status.as_u16(), contract.admin_forbidden.status);
    assert_contract_json(
        &json,
        &contract.admin_forbidden.json,
        "admin_token_create_non_admin_forbidden",
    );
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
async fn admin_async_jobs_match_shared_contract() {
    let contract = load_contract();
    let app = build_app_with_admin();
    let auth = &[("authorization", "Bearer admin-token")];

    for (label, path, q, detail_key) in [
        (
            "prompt_optimize",
            "/admin/prompts/optimize",
            &contract.admin_prompt_optimize,
            "message",
        ),
        (
            "feedback_export",
            "/admin/feedback/export",
            &contract.admin_feedback_export,
            "download_url",
        ),
    ] {
        let (status, json) = post_json(app.clone(), path, auth, q.request.clone()).await;
        assert_eq!(status.as_u16(), q.status, "{label}");
        assert_eq!(json["status"], q.json["status"], "{label}");
        assert_eq!(json[detail_key], q.json[detail_key], "{label}");
        assert!(
            Uuid::parse_str(json["job_id"].as_str().unwrap()).is_ok(),
            "{label}"
        );
    }
}

#[tokio::test]
async fn admin_feedback_stats_variants_match_shared_contract() {
    let contract = load_contract();
    let app = build_app_with_admin();
    let auth = &[("authorization", "Bearer admin-token")];

    for (label, path, expected) in [
        (
            "default",
            "/admin/feedback/stats?agent_id=contract-agent",
            &contract.admin_feedback_stats,
        ),
        (
            "filtered",
            "/admin/feedback/stats?agent_id=contract-agent&since=2026-01-04%2000:00:00",
            &contract.admin_feedback_stats_filtered,
        ),
    ] {
        let (status, json) = read_json(app.clone(), path, auth).await;
        assert_eq!(status.as_u16(), expected.status, "{label}");
        assert_contract_json(&json, &expected.json, label);
    }
}

#[tokio::test]
async fn admin_role_grant_variants_match_shared_contract() {
    let contract = load_contract();
    let app = build_app_with_admin();
    let auth = &[("authorization", "Bearer admin-token")];

    for (label, q) in [
        ("grant", &contract.admin_role_grant),
        ("grant_existing", &contract.admin_role_grant_existing),
        (
            "grant_user_not_found",
            &contract.admin_role_grant_user_not_found,
        ),
        (
            "grant_role_not_found",
            &contract.admin_role_grant_role_not_found,
        ),
    ] {
        let (status, json) = post_json(
            app.clone(),
            "/admin/users/grant-role",
            auth,
            q.request.clone(),
        )
        .await;
        assert_eq!(status.as_u16(), q.status, "{label}");
        assert_contract_json(&json, &q.json, label);
    }
}

#[tokio::test]
async fn admin_role_revoke_variants_match_shared_contract() {
    let contract = load_contract();
    let app = build_app_with_admin();
    let auth = &[("authorization", "Bearer admin-token")];

    for (label, q) in [
        ("revoke", &contract.admin_role_revoke),
        ("revoke_missing", &contract.admin_role_revoke_missing),
        (
            "revoke_user_not_found",
            &contract.admin_role_revoke_user_not_found,
        ),
        (
            "revoke_role_not_found",
            &contract.admin_role_revoke_role_not_found,
        ),
    ] {
        let (status, json) = post_json(
            app.clone(),
            "/admin/users/revoke-role",
            auth,
            q.request.clone(),
        )
        .await;
        assert_eq!(status.as_u16(), q.status, "{label}");
        assert_contract_json(&json, &q.json, label);
    }
}

#[tokio::test]
async fn admin_tokens_variants_match_shared_contract() {
    let contract = load_contract();
    let app = build_app_with_admin();
    let auth = &[("authorization", "Bearer admin-token")];

    for (label, path, expected) in [
        ("all", "/admin/tokens", &contract.admin_tokens),
        (
            "llm_global",
            "/admin/tokens?token_type=llm&scope=global",
            &contract.admin_tokens_llm_global,
        ),
    ] {
        let (status, json) = read_json(app.clone(), path, auth).await;
        assert_eq!(status.as_u16(), expected.status, "{label}");
        assert_contract_json(&json, &expected.json, label);
    }
}

#[tokio::test]
async fn admin_audit_variants_match_shared_contract() {
    let contract = load_contract();
    let app = build_app_with_admin();
    let auth = &[("authorization", "Bearer admin-token")];

    for (label, path, expected) in [
        ("default", "/admin/audit", &contract.admin_audit),
        (
            "user_filtered",
            "/admin/audit?user_id=contract-audit-user&limit=1",
            &contract.admin_audit_user_filtered,
        ),
    ] {
        let (status, json) = read_json(app.clone(), path, auth).await;
        assert_eq!(status.as_u16(), expected.status, "{label}");
        assert_contract_json(&json, &expected.json, label);
    }
}

#[tokio::test]
async fn model_router_operations_require_admin_and_durable_storage() {
    let app = build_app_with_admin();
    for (headers, expected) in [
        (vec![], StatusCode::UNAUTHORIZED),
        (
            vec![("authorization", "Bearer user-token")],
            StatusCode::FORBIDDEN,
        ),
        (
            vec![("authorization", "Bearer admin-token")],
            StatusCode::SERVICE_UNAVAILABLE,
        ),
    ] {
        let (status, _) = read_json(app.clone(), "/admin/model-router/owner-1", &headers).await;
        assert_eq!(status, expected);
        for (action, payload) in [
            (
                "rollback",
                serde_json::json!({"expected_revision":1,"reason":"manual_kill"}),
            ),
            (
                "revoke",
                serde_json::json!({"expected_revision":1,"source_ids":["source-1"]}),
            ),
            (
                "canary",
                serde_json::json!({"expected_revision":1,"basis_points":100}),
            ),
        ] {
            let (status, _) = post_json(
                app.clone(),
                &format!("/admin/model-router/owner-1/{action}"),
                &headers,
                payload,
            )
            .await;
            assert_eq!(status, expected);
        }
    }
}

#[path = "../../services/tests/common/isolated_database.rs"]
mod isolated_router_database;
#[path = "../../services/tests/common/mod.rs"]
mod rollout_db_common;
#[path = "../../services/tests/common/router_deployment.rs"]
mod rollout_db_fixture;

#[tokio::test]
#[ignore = "requires ASTRA_TEST_DB_IT=1 and explicitly isolated ASTRA_TEST_DATABASE"]
async fn model_router_http_dashboard_canary_outcome_and_rollback_on_matrixone() {
    use astra_services::runs::{DatabaseRunStateStore, RunStateStore};
    use astra_services::tuning::rollout::*;
    isolated_router_database::require_isolated_database(
        &rollout_db_common::require_db_it_env().database,
    );
    let pool = rollout_db_common::setup_pool().await;
    let owner = format!("router-{}", Uuid::new_v4());
    let session = Uuid::new_v4().to_string();
    let run_id = Uuid::new_v4().to_string();
    let mut d = rollout_db_fixture::deployment(&owner);
    d.review.minimum_shadow_sessions = 1;
    let deployment_id = d.deployment_id.clone();
    let registry = DatabaseRouterRolloutStore(pool.clone());
    registry
        .change(
            &owner,
            0,
            "admin-1",
            RolloutChange::Publish(Box::new(d.clone())),
        )
        .await
        .unwrap();
    let app = build_app(
        AppState::new(ServiceInfo::default(), Arc::new(StubHealthChecker))
            .with_admin_authorizer(Arc::new(StubAdminAuthorizer))
            .with_shared_pool(pool.clone()),
    );
    let headers = [("authorization", "Bearer admin-token")];
    let base = format!("/admin/model-router/{owner}");
    // Missing shadow evidence cannot advance the deployment.
    let (status, _) = post_json(
        app.clone(),
        &format!("{base}/canary"),
        &headers,
        serde_json::json!({"expected_revision":1,"basis_points":100}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    sqlx::query("INSERT INTO agent_sessions (user_id, session_id, status, created_at, updated_at, last_active_at) VALUES (?, ?, 'active', NOW(6), NOW(6), NOW(6))").bind(&owner).bind(&session).execute(pool.get()).await.unwrap();
    let store = Arc::new(DatabaseRunStateStore::new(pool.clone()));
    let engine = astra_runtime::server::run::engine::RunEngine::new(store.clone());
    let mut context = astra_runtime::server::run::engine::RunStartContext::default();
    context.model_selection = Some(astra_turn_types::ModelSelection {
        offering_id: "strong".into(),
    });
    context.resolved_model_selection = Some(astra_services::runs::ResolvedModelSelection {
        offering_id: "strong".into(),
        model_name: "strong-model".into(),
    });
    let authority = engine
        .start_run_with_context(&run_id, &owner, &session, context.clone())
        .await
        .unwrap();
    let mut decision =
        serde_json::from_str::<astra_services::evaluation::router::RouterDatasetInput>(
            include_str!("../../../fixtures/contracts/model_router_offline.json"),
        )
        .unwrap()
        .sources
        .remove(0)
        .decision;
    decision.run_id = run_id.clone();
    decision.session_id = session.clone();
    decision.policy.strong_offering_id = "strong".into();
    decision.policy.economy_offering_id = "economy".into();
    decision.selected_offering_id = "strong".into();
    decision.selected_model = "strong-model".into();
    decision.rollout = Some(RouterRolloutDecision {
        deployment_id: deployment_id.clone(),
        revision: 1,
        candidate_sha256: d.tuning.candidate_sha256,
        rubric_version: "rubric-1".into(),
        routing_failure: None,
        cohort: RolloutCohort::Shadow,
        cohort_probability_basis_points: 10_000,
        proposed_offering_id: "economy".into(),
        abstained: false,
        admission_rejected: false,
        routing_overhead_us: 500,
    });
    let event = serde_json::json!({"event_type":astra_services::model_routing::EVENT_TYPE,"idempotency_key":astra_services::model_routing::DECISION_KEY,"data":decision.clone()});
    assert!(
        store
            .append_events_if_current_generation_and_status(
                &owner,
                &session,
                &run_id,
                authority.owner_generation,
                &["running"],
                &[event]
            )
            .await
            .unwrap()
    );
    // Database event time may precede the host's deployment timestamp. Exact
    // owner/deployment membership must survive clock skew.
    sqlx::query("UPDATE agent_run_events SET created_at = ? WHERE user_id = ? AND run_id = ?")
        .bind((chrono::Utc::now() - chrono::Duration::minutes(1)).naive_utc())
        .bind(&owner)
        .bind(&run_id)
        .execute(pool.get())
        .await
        .unwrap();
    let (status, dashboard) = read_json(app.clone(), &base, &headers).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(dashboard["cohorts"]["shadow"]["sessions"], 1);
    assert_eq!(dashboard["cohorts"]["shadow"]["known_quality"], 0);
    assert!(dashboard["cohorts"]["shadow"]["cost_per_acceptable_task"].is_null());
    let (status, _) = post_json(
        app.clone(),
        &format!("{base}/canary"),
        &headers,
        serde_json::json!({"expected_revision":1,"basis_points":100}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let outcome = serde_json::json!({"rubric_version":"rubric-1","evidence_reference":"verifier-1","acceptable":false,"corrected":true,"full_episode_cost_usd":0.2,"episode_latency_ms":1000,"critical_violation":true});
    let path = format!("{base}/outcomes/{run_id}");
    assert_eq!(
        post_json(app.clone(), &path, &headers, outcome.clone())
            .await
            .0,
        StatusCode::BAD_REQUEST
    );
    // The terminal row is owned by the existing run lifecycle. Only fixture
    // settlement uses SQL here; outcome append still exercises its fenced API.
    sqlx::query("UPDATE agent_runs SET status = 'completed' WHERE user_id = ? AND run_id = ?")
        .bind(&owner)
        .bind(&run_id)
        .execute(pool.get())
        .await
        .unwrap();
    let response = app
        .clone()
        .oneshot(build_request_with_json(
            "POST",
            &path,
            &headers,
            outcome.clone(),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    assert_eq!(
        registry
            .load(&owner)
            .await
            .unwrap()
            .deployment
            .unwrap()
            .mode,
        RolloutMode::RolledBack
    );
    let response = app
        .clone()
        .oneshot(build_request_with_json(
            "POST",
            &path,
            &headers,
            outcome.clone(),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let mut conflict_outcome = outcome.clone();
    conflict_outcome["acceptable"] = serde_json::json!(true);
    let response = app
        .clone()
        .oneshot(build_request_with_json(
            "POST",
            &path,
            &headers,
            conflict_outcome,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CONFLICT);
    let (_, dashboard) = read_json(app.clone(), &base, &headers).await;
    assert_eq!(dashboard["cohorts"]["shadow"]["known_quality"], 1);
    assert_eq!(dashboard["cohorts"]["shadow"]["critical_violations"], 1);
    // A failed treatment is still reviewable after its deployment is replaced.
    let late_run_id = Uuid::new_v4().to_string();
    let late_session = Uuid::new_v4().to_string();
    sqlx::query("INSERT INTO agent_sessions (user_id, session_id, status, created_at, updated_at, last_active_at) VALUES (?, ?, 'active', NOW(6), NOW(6), NOW(6))").bind(&owner).bind(&late_session).execute(pool.get()).await.unwrap();
    let late_authority = engine
        .start_run_with_context(&late_run_id, &owner, &late_session, context)
        .await
        .unwrap();
    decision.run_id = late_run_id.clone();
    decision.session_id = late_session.clone();
    decision.policy_version =
        astra_turn_types::model_routing::LEARNED_CANARY_ROUTING_POLICY_VERSION.into();
    let rollout = decision.rollout.as_mut().unwrap();
    rollout.cohort = RolloutCohort::Treatment;
    rollout.cohort_probability_basis_points = 100;
    rollout.routing_failure = Some(RouterRoutingFailure::OverheadBudgetExceeded);
    rollout.routing_overhead_us = 200_000;
    let late_event = serde_json::json!({"event_type":astra_services::model_routing::EVENT_TYPE,"idempotency_key":astra_services::model_routing::DECISION_KEY,"data":decision});
    assert!(
        store
            .append_events_if_current_generation_and_status(
                &owner,
                &late_session,
                &late_run_id,
                late_authority.owner_generation,
                &["running"],
                &[late_event]
            )
            .await
            .unwrap()
    );
    sqlx::query("UPDATE agent_runs SET status = 'failed' WHERE user_id = ? AND run_id = ?")
        .bind(&owner)
        .bind(&late_run_id)
        .execute(pool.get())
        .await
        .unwrap();
    let (_, report) = read_json(app.clone(), &base, &headers).await;
    assert_eq!(report["cohorts"]["treatment"]["routing_failures"], 1);
    assert_eq!(report["cohorts"]["treatment"]["known_quality"], 0);
    let mut replacement = rollout_db_fixture::deployment(&owner);
    replacement.rubric_version = "rubric-2".into();
    let replacement_id = replacement.deployment_id.clone();
    registry
        .change(
            &owner,
            3,
            "admin",
            RolloutChange::Publish(Box::new(replacement)),
        )
        .await
        .unwrap();
    let late_path = format!("{base}/outcomes/{late_run_id}");
    let mut wrong_rubric = outcome.clone();
    wrong_rubric["rubric_version"] = serde_json::json!("rubric-2");
    assert_eq!(
        post_json(app.clone(), &late_path, &headers, wrong_rubric)
            .await
            .0,
        StatusCode::BAD_REQUEST
    );
    for (report_path, expected) in [
        (&late_path, StatusCode::CREATED),
        (&late_path, StatusCode::OK),
        (&path, StatusCode::OK),
    ] {
        let response = app
            .clone()
            .oneshot(build_request_with_json(
                "POST",
                report_path,
                &headers,
                outcome.clone(),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), expected);
    }
    let current = registry.load(&owner).await.unwrap();
    assert_eq!(current.revision, 4);
    assert_eq!(
        current.deployment.as_ref().unwrap().deployment_id,
        replacement_id
    );
    assert_eq!(
        current.deployment.as_ref().unwrap().mode,
        RolloutMode::Shadow
    );
    let late_saved = store
        .load_run_event_by_idempotency_key(&owner, &late_run_id, OUTCOME_EVENT, OUTCOME_KEY)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(late_saved["data"]["deployment_id"], deployment_id);
    assert_eq!(late_saved["data"]["outcome"]["rubric_version"], "rubric-1");
    let (status, _) = post_json(
        app,
        &format!("{base}/revoke"),
        &headers,
        serde_json::json!({"expected_revision":4,"source_ids":["source-1"]}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    for table in [
        "agent_run_events",
        "agent_runs",
        "agent_sessions",
        "model_router_deployments",
    ] {
        sqlx::query(&format!("DELETE FROM {table} WHERE user_id = ?"))
            .bind(&owner)
            .execute(pool.get())
            .await
            .unwrap();
    }
    sqlx::query(
        "DELETE FROM auth_audit_logs WHERE resource_type = 'model_router' AND resource_id IN (?, ?)",
    )
    .bind(&replacement_id)
    .bind(&deployment_id)
    .execute(pool.get())
    .await
    .unwrap();
}
