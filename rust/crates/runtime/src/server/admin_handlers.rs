use super::*;

pub(super) async fn admin_register_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<AuthRegisterRequest>,
) -> Result<(StatusCode, Json<AuthRegisterResponse>), (StatusCode, Json<ErrorResponse>)> {
    let admin_exists = state
        .admin
        .user_role_manager
        .has_role_members("astra_admin")
        .await?;

    if admin_exists {
        state.admin.authorizer.require_admin(&headers).await?;
    }

    let username = request.username.clone();
    let password = request.password.clone();
    let user = state
        .auth_service
        .register(AuthRegisterRequestData {
            username: request.username,
            email: request.email,
            password: request.password,
            display_name: request.display_name,
        })
        .await?;

    state
        .admin
        .user_role_manager
        .grant_role(AdminUserRoleRequestData {
            username: user.username.clone(),
            role_name: "astra_admin".to_string(),
        })
        .await?;

    let tokens = state
        .auth_service
        .login(AuthLoginRequestData { username, password })
        .await?;

    let mut auth_headers = HeaderMap::new();
    if let Ok(value) = HeaderValue::from_str(&format!("Bearer {}", tokens.access_token)) {
        auth_headers.insert("authorization", value);
    }
    if state
        .admin
        .authorizer
        .require_admin(&auth_headers)
        .await
        .is_err()
    {
        return Err(error_response(
            StatusCode::CONFLICT,
            "Admin bootstrap already exists; log in as an existing admin to create another admin",
        ));
    }

    Ok((
        StatusCode::CREATED,
        Json(AuthRegisterResponse {
            user_id: user.user_id,
            username: user.username,
            email: user.email,
            display_name: user.display_name,
            roles: vec!["astra_user".to_string(), "astra_admin".to_string()],
            is_admin: true,
            access_token: tokens.access_token,
            refresh_token: tokens.refresh_token,
            token_type: tokens.token_type,
            expires_in: tokens.expires_in,
        }),
    ))
}

pub(super) async fn admin_init_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<AdminInitResponse>, (StatusCode, Json<ErrorResponse>)> {
    state.admin.authorizer.require_admin(&headers).await?;
    let result = state.admin.initializer.initialize().await?;
    Ok(Json(AdminInitResponse::from(result)))
}

pub(super) async fn admin_list_tokens_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<AdminTokenListQuery>,
) -> Result<Json<Vec<AdminTokenResponse>>, (StatusCode, Json<ErrorResponse>)> {
    state.admin.authorizer.require_admin(&headers).await?;
    let tokens = state
        .admin
        .token_reader
        .list_tokens(AdminTokenFilter {
            token_type: query.token_type,
            scope: query.scope,
        })
        .await?;

    Ok(Json(
        tokens.into_iter().map(AdminTokenResponse::from).collect(),
    ))
}

pub(super) async fn admin_create_token_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<AdminTokenCreateRequest>,
) -> Result<(StatusCode, Json<AdminTokenResponse>), (StatusCode, Json<ErrorResponse>)> {
    state.admin.authorizer.require_admin(&headers).await?;
    let created = state
        .admin
        .token_writer
        .create_token(AdminTokenCreateRequestData {
            token_type: request.token_type,
            provider: request.provider,
            scope: request.scope,
            scope_id: request.scope_id,
            token_value: request.token_value,
        })
        .await?;

    Ok((StatusCode::CREATED, Json(AdminTokenResponse::from(created))))
}

pub(super) async fn admin_audit_logs_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<AdminAuditListQuery>,
) -> Result<Json<Vec<AdminAuditResponse>>, (StatusCode, Json<ErrorResponse>)> {
    state.admin.authorizer.require_admin(&headers).await?;
    let logs = state
        .admin
        .audit_reader
        .list_audit_logs(AdminAuditFilter {
            user_id: query.user_id,
            since: query.since,
            limit: query.limit,
        })
        .await?;

    Ok(Json(
        logs.into_iter().map(AdminAuditResponse::from).collect(),
    ))
}

pub(super) async fn admin_prompt_optimize_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<PromptOptimizeRequest>,
) -> Result<Json<PromptOptimizeResponse>, (StatusCode, Json<ErrorResponse>)> {
    state.admin.authorizer.require_admin(&headers).await?;
    let _ = request.optimization_type;
    Ok(Json(PromptOptimizeResponse {
        job_id: Uuid::new_v4().to_string(),
        status: "queued",
        message: format!(
            "Prompt optimization job queued for agent {}",
            request.agent_id
        ),
    }))
}

pub(super) async fn admin_feedback_export_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<FeedbackExportRequest>,
) -> Result<Json<FeedbackExportResponse>, (StatusCode, Json<ErrorResponse>)> {
    state.admin.authorizer.require_admin(&headers).await?;
    let _ = request.agent_id;
    let _ = request.format;
    Ok(Json(FeedbackExportResponse {
        job_id: Uuid::new_v4().to_string(),
        status: "queued",
        download_url: None,
    }))
}

pub(super) async fn admin_feedback_stats_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<AdminFeedbackStatsQuery>,
) -> Result<Json<AdminFeedbackStatsResponse>, (StatusCode, Json<ErrorResponse>)> {
    state.admin.authorizer.require_admin(&headers).await?;
    let stats = state
        .admin
        .feedback_stats_reader
        .read_feedback_stats(AdminFeedbackStatsFilter {
            agent_id: query.agent_id,
            since: query.since,
        })
        .await?;

    Ok(Json(AdminFeedbackStatsResponse::from(stats)))
}

pub(super) async fn admin_grant_role_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<AdminUserRoleRequest>,
) -> Result<Json<AdminUserRoleResponse>, (StatusCode, Json<ErrorResponse>)> {
    state.admin.authorizer.require_admin(&headers).await?;
    let result = state
        .admin
        .user_role_manager
        .grant_role(AdminUserRoleRequestData {
            username: request.username,
            role_name: request.role_name,
        })
        .await?;

    Ok(Json(AdminUserRoleResponse::from(result)))
}

pub(super) async fn admin_revoke_role_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<AdminUserRoleRequest>,
) -> Result<Json<AdminUserRoleResponse>, (StatusCode, Json<ErrorResponse>)> {
    state.admin.authorizer.require_admin(&headers).await?;
    let result = state
        .admin
        .user_role_manager
        .revoke_role(AdminUserRoleRequestData {
            username: request.username,
            role_name: request.role_name,
        })
        .await?;

    Ok(Json(AdminUserRoleResponse::from(result)))
}

/// POST /admin/cleanup — trigger immediate expired data cleanup.
pub(super) async fn admin_cleanup_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
    state.admin.authorizer.require_admin(&headers).await?;

    let pool = state
        .shared_pool
        .as_ref()
        .ok_or_else(|| internal_error("database pool not available"))?;

    let policy = astra_services::RetentionPolicy::default();
    let results = astra_services::cleanup_expired_data(pool.get(), &policy)
        .await
        .map_err(internal_error)?;

    let total: u64 = results.iter().map(|r| r.rows_deleted).sum();
    let details: Vec<serde_json::Value> = results
        .iter()
        .map(|r| {
            serde_json::json!({
                "table": r.table,
                "rows_deleted": r.rows_deleted,
            })
        })
        .collect();

    Ok(Json(serde_json::json!({
        "total_deleted": total,
        "tables": details,
    })))
}

// ── Runtime tool enable/disable ───────────────────────────────────────────────

#[derive(serde::Deserialize)]
pub(super) struct DisableToolRequest {
    tool_name: String,
}

/// GET /admin/tools/disabled — list all runtime-disabled tools.
pub(super) async fn admin_list_disabled_tools_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Vec<String>>, (StatusCode, Json<ErrorResponse>)> {
    state.admin.authorizer.require_admin(&headers).await?;
    let tools = state.tool_execution_service.disabled_tools().await;
    Ok(Json(tools))
}

/// PUT /admin/tools/disabled — disable a tool at runtime.
pub(super) async fn admin_disable_tool_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<DisableToolRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
    state.admin.authorizer.require_admin(&headers).await?;
    let added = state
        .tool_execution_service
        .disable_tool(&body.tool_name)
        .await;
    Ok(Json(serde_json::json!({
        "tool_name": body.tool_name,
        "was_added": added,
    })))
}

/// DELETE /admin/tools/disabled/{tool_name} — re-enable a tool at runtime.
pub(super) async fn admin_enable_tool_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(tool_name): Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
    state.admin.authorizer.require_admin(&headers).await?;
    let removed = state.tool_execution_service.enable_tool(&tool_name).await;
    Ok(Json(serde_json::json!({
        "tool_name": tool_name,
        "was_removed": removed,
    })))
}
