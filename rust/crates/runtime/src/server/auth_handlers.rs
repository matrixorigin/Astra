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
    method: reqwest::Method,
    endpoint: &str,
    body: serde_json::Value,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(headers).await?;
    let user_id = user.user_id.clone();
    let inject_identity = should_inject_memory_proxy_identity(endpoint);
    let body = apply_memory_proxy_identity(body, &user_id, inject_identity, endpoint);

    state
        .memoria_forwarder
        .forward(method, endpoint, body)
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
            } else if let Some(status) = parse_memoria_forward_status(&error) {
                error_response(status, &error)
            } else {
                internal_error(&error)
            }
        })
}

fn parse_memoria_forward_status(error: &str) -> Option<StatusCode> {
    let suffix = error.strip_prefix("Memoria error ")?;
    let code = suffix.split_whitespace().next()?.parse::<u16>().ok()?;
    StatusCode::from_u16(code).ok()
}

fn should_inject_memory_proxy_identity(endpoint: &str) -> bool {
    !endpoint.ends_with("/purge")
}

fn apply_memory_proxy_identity(
    mut body: serde_json::Value,
    user_id: &str,
    inject_identity: bool,
    endpoint: &str,
) -> serde_json::Value {
    if inject_identity && let Some(obj) = body.as_object_mut() {
        // Force-overwrite both ownership fields so clients cannot spoof either
        // another user's identity or a foreign session scope inside Memoria.
        obj.insert(
            "session_id".to_string(),
            serde_json::Value::String(user_id.to_string()),
        );
        obj.insert(
            "user_id".to_string(),
            serde_json::Value::String(user_id.to_string()),
        );
    }

    // Memoria PurgeRequest only accepts: memory_ids, topic, reason.
    // Strip injected fields that would cause a 422 Unprocessable Entity.
    if endpoint.ends_with("/purge")
        && let Some(obj) = body.as_object_mut()
    {
        obj.remove("session_id");
        obj.remove("user_id");
    }

    body
}

fn apply_memoria_management_identity(
    mut body: serde_json::Value,
    user_id: &str,
) -> serde_json::Value {
    if let Some(obj) = body.as_object_mut() {
        obj.insert(
            "user_id".to_string(),
            serde_json::Value::String(user_id.to_string()),
        );
    }
    body
}

pub(super) async fn memory_proxy_store_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
    memory_proxy_call(
        &state,
        &headers,
        reqwest::Method::POST,
        "/v1/memories",
        body,
    )
    .await
}

pub(super) async fn memory_proxy_retrieve_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
    memory_proxy_call(
        &state,
        &headers,
        reqwest::Method::POST,
        "/v1/memories/retrieve",
        body,
    )
    .await
}

pub(super) async fn memory_proxy_search_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
    memory_proxy_call(
        &state,
        &headers,
        reqwest::Method::POST,
        "/v1/memories/search",
        body,
    )
    .await
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

    let result = memory_proxy_call(
        &state,
        &headers,
        reqwest::Method::POST,
        "/v1/memories/purge",
        body,
    )
    .await?;
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

pub(super) async fn memory_proxy_expand_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(memory_id): Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
    memory_proxy_call(
        &state,
        &headers,
        reqwest::Method::GET,
        &format!("/v1/memories/{memory_id}"),
        serde_json::json!({}),
    )
    .await
}

pub(super) async fn memory_proxy_correct_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
    memory_proxy_call(
        &state,
        &headers,
        reqwest::Method::POST,
        "/v1/memories/correct",
        body,
    )
    .await
}

pub(super) async fn memory_proxy_correct_by_id_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(memory_id): Path<String>,
    Json(body): Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
    memory_proxy_call(
        &state,
        &headers,
        reqwest::Method::PUT,
        &format!("/v1/memories/{memory_id}/correct"),
        body,
    )
    .await
}

pub(super) async fn memory_proxy_feedback_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(memory_id): Path<String>,
    Json(body): Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
    memory_proxy_call(
        &state,
        &headers,
        reqwest::Method::POST,
        &format!("/v1/memories/{memory_id}/feedback"),
        body,
    )
    .await
}

pub(super) async fn memory_proxy_profile_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
    memory_proxy_call(
        &state,
        &headers,
        reqwest::Method::GET,
        "/v1/profiles/me",
        serde_json::json!({}),
    )
    .await
}

pub(super) async fn memory_proxy_reflect_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
    memory_proxy_call(&state, &headers, reqwest::Method::POST, "/v1/reflect", body).await
}

async fn memoria_management_proxy_call(
    state: &AppState,
    headers: &HeaderMap,
    method: reqwest::Method,
    endpoint: &str,
    body: Option<serde_json::Value>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(headers).await?;
    let body = apply_memoria_management_identity(
        body.unwrap_or_else(|| serde_json::json!({})),
        &user.user_id,
    );
    state
        .memoria_forwarder
        .forward(method, endpoint, body)
        .await
        .map(Json)
        .map_err(|error| {
            tracing::warn!(
                target: "astra_runtime::auth",
                endpoint = endpoint,
                error = %error,
                "memoria management proxy forward failed"
            );
            if error.contains("not configured") {
                error_response(StatusCode::SERVICE_UNAVAILABLE, &error)
            } else {
                internal_error(&error)
            }
        })
}

pub(super) async fn memoria_proxy_snapshot_create_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
    memoria_management_proxy_call(
        &state,
        &headers,
        reqwest::Method::POST,
        "/v1/snapshots",
        Some(body),
    )
    .await
}

pub(super) async fn memoria_proxy_snapshots_list_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
    memoria_management_proxy_call(
        &state,
        &headers,
        reqwest::Method::GET,
        "/v1/snapshots",
        None,
    )
    .await
}

pub(super) async fn memoria_proxy_snapshot_rollback_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(name): Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
    memoria_management_proxy_call(
        &state,
        &headers,
        reqwest::Method::POST,
        &format!("/v1/snapshots/{name}/rollback"),
        None,
    )
    .await
}

pub(super) async fn memoria_proxy_snapshot_diff_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(name): Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
    memoria_management_proxy_call(
        &state,
        &headers,
        reqwest::Method::GET,
        &format!("/v1/snapshots/{name}/diff"),
        None,
    )
    .await
}

pub(super) async fn memoria_proxy_branch_create_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
    memoria_management_proxy_call(
        &state,
        &headers,
        reqwest::Method::POST,
        "/v1/branches",
        Some(body),
    )
    .await
}

pub(super) async fn memoria_proxy_branches_list_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
    memoria_management_proxy_call(&state, &headers, reqwest::Method::GET, "/v1/branches", None)
        .await
}

pub(super) async fn memoria_proxy_branch_checkout_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(name): Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
    memoria_management_proxy_call(
        &state,
        &headers,
        reqwest::Method::POST,
        &format!("/v1/branches/{name}/checkout"),
        None,
    )
    .await
}

pub(super) async fn memoria_proxy_branch_merge_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(name): Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
    memoria_management_proxy_call(
        &state,
        &headers,
        reqwest::Method::POST,
        &format!("/v1/branches/{name}/merge"),
        None,
    )
    .await
}

pub(super) async fn memoria_proxy_branch_diff_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(name): Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
    memoria_management_proxy_call(
        &state,
        &headers,
        reqwest::Method::GET,
        &format!("/v1/branches/{name}/diff"),
        None,
    )
    .await
}

pub(super) async fn memoria_proxy_health_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
    memoria_management_proxy_call(
        &state,
        &headers,
        reqwest::Method::GET,
        "/v1/health/analyze",
        None,
    )
    .await
}

pub(super) async fn memoria_proxy_governance_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
    memoria_management_proxy_call(
        &state,
        &headers,
        reqwest::Method::POST,
        "/v1/governance",
        Some(body),
    )
    .await
}

pub(super) async fn memoria_proxy_consolidate_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
    memoria_management_proxy_call(
        &state,
        &headers,
        reqwest::Method::POST,
        "/v1/consolidate",
        Some(body),
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::{
        apply_memoria_management_identity, apply_memory_proxy_identity,
        parse_memoria_forward_status, should_inject_memory_proxy_identity,
    };
    use axum::http::StatusCode;
    use serde_json::json;

    #[test]
    fn apply_memory_proxy_identity_overwrites_spoofed_user_and_session() {
        let body = json!({
            "content": "probe",
            "memory_type": "semantic",
            "user_id": "spoofed-user",
            "session_id": "spoofed-session"
        });

        let out = apply_memory_proxy_identity(body, "real-user", true, "/v1/memories");

        assert_eq!(out["user_id"].as_str(), Some("real-user"));
        assert_eq!(out["session_id"].as_str(), Some("real-user"));
    }

    #[test]
    fn apply_memory_proxy_identity_strips_injected_fields_for_purge() {
        let body = json!({
            "memory_ids": ["m1"],
            "user_id": "spoofed-user",
            "session_id": "spoofed-session"
        });

        let out = apply_memory_proxy_identity(body, "real-user", true, "/v1/memories/purge");

        assert!(out.get("user_id").is_none());
        assert!(out.get("session_id").is_none());
        assert_eq!(out["memory_ids"], json!(["m1"]));
    }

    #[test]
    fn memory_proxy_identity_policy_only_skips_purge() {
        assert!(should_inject_memory_proxy_identity("/v1/memories"));
        assert!(should_inject_memory_proxy_identity(
            "/v1/memories/m-1/feedback"
        ));
        assert!(should_inject_memory_proxy_identity("/v1/profiles/me"));
        assert!(!should_inject_memory_proxy_identity("/v1/memories/purge"));
    }

    #[test]
    fn memoria_management_identity_injects_user_only() {
        let out = apply_memoria_management_identity(json!({"name": "snap-1"}), "real-user");

        assert_eq!(out["user_id"].as_str(), Some("real-user"));
        assert!(out.get("session_id").is_none());
        assert_eq!(out["name"].as_str(), Some("snap-1"));
    }

    #[test]
    fn parse_memoria_forward_status_extracts_downstream_http_code() {
        assert_eq!(
            parse_memoria_forward_status(
                "Memoria error 422 Unprocessable Entity: Invalid memory type: session_memory"
            ),
            Some(StatusCode::UNPROCESSABLE_ENTITY)
        );
        assert_eq!(
            parse_memoria_forward_status("Memoria error 500 Internal Server Error: boom"),
            Some(StatusCode::INTERNAL_SERVER_ERROR)
        );
        assert_eq!(parse_memoria_forward_status("random error"), None);
    }
}
