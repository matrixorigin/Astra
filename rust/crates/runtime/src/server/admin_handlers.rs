use super::*;

pub(super) async fn admin_init_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<AdminInitResponse>, (StatusCode, Json<ErrorResponse>)> {
    state.admin_authorizer.require_admin(&headers).await?;
    let result = state.admin_initializer.initialize().await?;
    Ok(Json(AdminInitResponse::from(result)))
}

pub(super) async fn admin_list_tokens_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<AdminTokenListQuery>,
) -> Result<Json<Vec<AdminTokenResponse>>, (StatusCode, Json<ErrorResponse>)> {
    state.admin_authorizer.require_admin(&headers).await?;
    let tokens = state
        .admin_token_reader
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
    state.admin_authorizer.require_admin(&headers).await?;
    let created = state
        .admin_token_writer
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
    state.admin_authorizer.require_admin(&headers).await?;
    let logs = state
        .admin_audit_reader
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
    state.admin_authorizer.require_admin(&headers).await?;
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
    state.admin_authorizer.require_admin(&headers).await?;
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
    state.admin_authorizer.require_admin(&headers).await?;
    let stats = state
        .admin_feedback_stats_reader
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
    state.admin_authorizer.require_admin(&headers).await?;
    let result = state
        .admin_user_role_manager
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
    state.admin_authorizer.require_admin(&headers).await?;
    let result = state
        .admin_user_role_manager
        .revoke_role(AdminUserRoleRequestData {
            username: request.username,
            role_name: request.role_name,
        })
        .await?;

    Ok(Json(AdminUserRoleResponse::from(result)))
}
