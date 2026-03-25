use super::*;

pub(super) async fn auth_register_handler(
    State(state): State<AppState>,
    Json(request): Json<AuthRegisterRequest>,
) -> Result<(StatusCode, Json<AuthRegisterResponse>), (StatusCode, Json<ErrorResponse>)> {
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
    let tokens = state
        .auth_service
        .login(AuthLoginRequestData { username, password })
        .await?;
    Ok((
        StatusCode::CREATED,
        Json(AuthRegisterResponse {
            user_id: user.user_id,
            username: user.username,
            email: user.email,
            display_name: user.display_name,
            access_token: tokens.access_token,
            refresh_token: tokens.refresh_token,
            token_type: tokens.token_type,
            expires_in: tokens.expires_in,
        }),
    ))
}

pub(super) async fn auth_login_handler(
    State(state): State<AppState>,
    Json(request): Json<AuthLoginRequest>,
) -> Result<Json<AuthTokenResponse>, (StatusCode, Json<ErrorResponse>)> {
    let tokens = state
        .auth_service
        .login(AuthLoginRequestData {
            username: request.username,
            password: request.password,
        })
        .await?;
    Ok(Json(AuthTokenResponse::from(tokens)))
}

pub(super) async fn auth_refresh_handler(
    State(state): State<AppState>,
    Json(request): Json<AuthRefreshRequest>,
) -> Result<Json<AuthTokenResponse>, (StatusCode, Json<ErrorResponse>)> {
    let tokens = state
        .auth_service
        .refresh(AuthRefreshRequestData {
            refresh_token: request.refresh_token,
        })
        .await?;
    Ok(Json(AuthTokenResponse::from(tokens)))
}

pub(super) async fn auth_logout_handler(
    State(state): State<AppState>,
    Json(request): Json<AuthRefreshRequest>,
) -> Result<Json<AuthLogoutResponse>, (StatusCode, Json<ErrorResponse>)> {
    state
        .auth_service
        .logout(AuthRefreshRequestData {
            refresh_token: request.refresh_token,
        })
        .await?;
    Ok(Json(AuthLogoutResponse {
        message: "Logged out successfully".to_string(),
    }))
}

pub(super) async fn auth_me_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<AuthUserResponse>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    Ok(Json(AuthUserResponse::from(user)))
}

async fn memory_proxy_call(
    state: &AppState,
    headers: &HeaderMap,
    endpoint: &str,
    mut body: serde_json::Value,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(headers).await?;
    let user_id = user.user_id.clone();

    if let Some(obj) = body.as_object_mut() {
        // Force-overwrite to prevent clients from impersonating other users
        obj.insert(
            "session_id".to_string(),
            serde_json::Value::String(user_id.clone()),
        );
        obj.insert(
            "user_id".to_string(),
            serde_json::Value::String(user_id.clone()),
        );
    }

    state
        .memoria_forwarder
        .forward(endpoint, body)
        .await
        .map(Json)
        .map_err(|error| {
            if error.contains("not configured") {
                error_response(StatusCode::SERVICE_UNAVAILABLE, &error)
            } else {
                internal_error(&error)
            }
        })
}

pub(super) async fn memory_proxy_store_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
    memory_proxy_call(&state, &headers, "/v1/memories", body).await
}

pub(super) async fn memory_proxy_retrieve_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
    memory_proxy_call(&state, &headers, "/v1/memories/retrieve", body).await
}

pub(super) async fn memory_proxy_search_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
    memory_proxy_call(&state, &headers, "/v1/memories/search", body).await
}

pub(super) async fn memory_proxy_purge_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
    memory_proxy_call(&state, &headers, "/v1/memories/purge", body).await
}
