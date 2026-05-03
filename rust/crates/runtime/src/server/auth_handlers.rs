use axum::extract::Extension;

use super::*;

pub(super) async fn auth_register_handler(
    Extension(trace): Extension<RequestTrace>,
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
    tracing::info!(
        target: "astra_runtime::auth",
        request_id = %trace.request_id,
        user_id = %user.user_id,
        "user registered"
    );
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
    Extension(trace): Extension<RequestTrace>,
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
    // Intentionally omit username: avoid PII in application logs (correlate via request_id / JWT).
    tracing::info!(
        target: "astra_runtime::auth",
        request_id = %trace.request_id,
        "login succeeded"
    );
    Ok(Json(AuthTokenResponse::from(tokens)))
}

pub(super) async fn auth_refresh_handler(
    Extension(trace): Extension<RequestTrace>,
    State(state): State<AppState>,
    Json(request): Json<AuthRefreshRequest>,
) -> Result<Json<AuthTokenResponse>, (StatusCode, Json<ErrorResponse>)> {
    let tokens = state
        .auth_service
        .refresh(AuthRefreshRequestData {
            refresh_token: request.refresh_token,
        })
        .await?;
    tracing::info!(
        target: "astra_runtime::auth",
        request_id = %trace.request_id,
        "access token refreshed"
    );
    Ok(Json(AuthTokenResponse::from(tokens)))
}

pub(super) async fn auth_logout_handler(
    Extension(trace): Extension<RequestTrace>,
    State(state): State<AppState>,
    Json(request): Json<AuthRefreshRequest>,
) -> Result<Json<AuthLogoutResponse>, (StatusCode, Json<ErrorResponse>)> {
    state
        .auth_service
        .logout(AuthRefreshRequestData {
            refresh_token: request.refresh_token,
        })
        .await?;
    tracing::info!(
        target: "astra_runtime::auth",
        request_id = %trace.request_id,
        "logout"
    );
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

    // Memoria PurgeRequest only accepts: memory_ids, topic, reason.
    // Strip injected fields that would cause a 422 Unprocessable Entity.
    if endpoint.ends_with("/purge") {
        if let Some(obj) = body.as_object_mut() {
            obj.remove("session_id");
            obj.remove("user_id");
        }
    }

    state
        .memoria_forwarder
        .forward(endpoint, body)
        .await
        .map(Json)
        .map_err(|error| {
            tracing::warn!(
                target: "astra_runtime::auth",
                endpoint = endpoint,
                error = %error,
                "memory proxy forward failed"
            );
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
    let filter = body
        .as_object()
        .and_then(|o| {
            o.get("topic")
                .or(o.get("session_id"))
                .or(o.get("memory_ids"))
        })
        .and_then(|v| v.as_str())
        .unwrap_or("unknown")
        .to_string();

    let result = memory_proxy_call(&state, &headers, "/v1/memories/purge", body).await?;
    let deleted = result
        .get("deleted_count")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let message = if deleted == 0 {
        format!("memory_purge: no entries matched filter [{filter}] (0 deleted)")
    } else {
        format!("memory_purge: deleted {deleted} entries matching [{filter}]")
    };
    let enriched = serde_json::json!({
        "status": "ok",
        "deleted_count": deleted,
        "message": message,
    });
    Ok(Json(enriched))
}
