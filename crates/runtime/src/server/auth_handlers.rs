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
    let mut auth_headers = HeaderMap::new();
    if let Ok(value) = HeaderValue::from_str(&format!("Bearer {}", tokens.access_token)) {
        auth_headers.insert("authorization", value);
    }
    let is_admin = state
        .admin
        .authorizer
        .require_admin(&auth_headers)
        .await
        .is_ok();
    let mut roles = vec!["astra_user".to_string()];
    if is_admin {
        roles.push("astra_admin".to_string());
    }
    tracing::info!(
        target: "astra_runtime::auth",
        request_id = %trace.request_id,
        user_id = %user.user_id,
        is_admin,
        "user registered"
    );
    Ok((
        StatusCode::CREATED,
        Json(AuthRegisterResponse {
            user_id: user.user_id,
            username: user.username,
            email: user.email,
            display_name: user.display_name,
            roles,
            is_admin,
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
    memory_proxy_call_for_user(state, &user.user_id, method, endpoint, body).await
}

async fn memory_proxy_call_for_user(
    state: &AppState,
    user_id: &str,
    method: reqwest::Method,
    endpoint: &str,
    body: serde_json::Value,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
    let requested_scope = memory_proxy_scope(&body, user_id)?;
    if let Some(scope) = requested_scope.as_ref() {
        ensure_memory_proxy_session_owner(state, scope).await?;
    }
    let strict_recall_scope = if is_strict_session_recall(endpoint, &body) {
        Some(requested_scope.clone().ok_or_else(|| {
            error_response(
                StatusCode::BAD_REQUEST,
                "strict session memory recall requires an owned session_id",
            )
        })?)
    } else {
        None
    };
    let inject_identity = should_inject_memory_proxy_identity(endpoint);
    let body = apply_memory_proxy_identity(body, user_id, inject_identity, endpoint);

    let response = state
        .memoria_forwarder
        .forward(method, endpoint, body)
        .await
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
        })?;

    if let Some(scope) = strict_recall_scope.as_ref()
        && let Err(error) = astra_memoria::validate_strict_recall_payload(&response, scope)
    {
        tracing::error!(
            target: "astra_runtime::auth",
            user_id = %scope.user_id,
            session_id = %scope.session_id,
            endpoint,
            error = %error,
            "Memoria returned content outside the authenticated session scope"
        );
        return Err(error_response(
            StatusCode::BAD_GATEWAY,
            "memory backend violated the requested session scope",
        ));
    }

    Ok(Json(response))
}

fn is_strict_session_recall(endpoint: &str, body: &serde_json::Value) -> bool {
    endpoint.ends_with("/retrieve")
        && body
            .get("session_scope")
            .and_then(serde_json::Value::as_str)
            == Some("only")
}

fn memory_proxy_scope(
    body: &serde_json::Value,
    user_id: &str,
) -> Result<Option<astra_memoria::MemoryScope>, (StatusCode, Json<ErrorResponse>)> {
    let Some(session_id_value) = body.get("session_id") else {
        return Ok(None);
    };
    let Some(session_id) = session_id_value.as_str() else {
        return Err(error_response(
            StatusCode::BAD_REQUEST,
            "memory session_id must be an exact string",
        ));
    };
    astra_memoria::MemoryScope::new(user_id, session_id)
        .map(Some)
        .map_err(|error| error_response(StatusCode::BAD_REQUEST, error))
}

async fn ensure_memory_proxy_session_owner(
    state: &AppState,
    scope: &astra_memoria::MemoryScope,
) -> Result<(), (StatusCode, Json<ErrorResponse>)> {
    let Some(shared_pool) = state.shared_pool.as_ref() else {
        return Err(error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "session ownership storage is unavailable for scoped memory operations",
        ));
    };
    let owned = astra_services::storage::agent_session_exists_for_user(
        shared_pool.get(),
        &scope.session_id,
        &scope.user_id,
    )
    .await
    .map_err(|error| internal_error(format!("memory session ownership check failed: {error}")))?;
    if !owned {
        return Err(error_response(
            StatusCode::NOT_FOUND,
            "memory session was not found for the authenticated user",
        ));
    }
    Ok(())
}

fn exact_memory_ids_for_user_purge(body: &serde_json::Value) -> Result<Vec<String>, &'static str> {
    const MAX_IDS: usize = 64;
    if body.get("topic").is_some() {
        return Err(
            "topic purge is not available through the multi-tenant user endpoint; delete exact memory_ids instead",
        );
    }
    let Some(ids) = body.get("memory_ids").and_then(serde_json::Value::as_array) else {
        return Err("memory purge requires a non-empty memory_ids array");
    };
    let mut seen = std::collections::HashSet::new();
    let mut exact = Vec::new();
    for id in ids {
        let Some(id) = id.as_str().map(str::trim).filter(|id| !id.is_empty()) else {
            return Err("memory_ids must contain only non-empty strings");
        };
        if seen.insert(id.to_string()) {
            exact.push(id.to_string());
        }
    }
    if exact.is_empty() {
        return Err("memory purge requires a non-empty memory_ids array");
    }
    if exact.len() > MAX_IDS {
        return Err("memory purge accepts at most 64 distinct memory_ids");
    }
    Ok(exact)
}

fn parse_memoria_forward_status(error: &str) -> Option<StatusCode> {
    let suffix = error.strip_prefix("Memoria error ")?;
    let code = suffix.split_whitespace().next()?.parse::<u16>().ok()?;
    StatusCode::from_u16(code).ok()
}

fn should_inject_memory_proxy_identity(endpoint: &str) -> bool {
    !endpoint.ends_with("/purge")
}

fn encode_memoria_memory_id(memory_id: &str) -> String {
    use percent_encoding::{NON_ALPHANUMERIC, utf8_percent_encode};
    utf8_percent_encode(memory_id, NON_ALPHANUMERIC).to_string()
}

fn apply_memory_proxy_identity(
    mut body: serde_json::Value,
    user_id: &str,
    inject_identity: bool,
    endpoint: &str,
) -> serde_json::Value {
    if inject_identity && let Some(obj) = body.as_object_mut() {
        // Authentication owns `user_id`; the durable session id remains a
        // separate, caller-selected identity that was authorized against the
        // session store before this function runs.
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
    let user = state.auth_service.current_user(&headers).await?;
    let memory_ids = exact_memory_ids_for_user_purge(&body)
        .map_err(|error| error_response(StatusCode::BAD_REQUEST, error))?;
    let mut deleted = 0_u64;
    for memory_id in memory_ids {
        let memory_id = encode_memoria_memory_id(&memory_id);
        let _deleted_response = memory_proxy_call_for_user(
            &state,
            &user.user_id,
            reqwest::Method::DELETE,
            &format!("/v1/memories/{memory_id}"),
            serde_json::json!({}),
        )
        .await?;
        deleted = deleted.saturating_add(1);
    }
    let enriched = serde_json::json!({
        "status": "ok",
        "deleted_count": deleted,
        "message": format!("memory_purge: deleted {deleted} exact entries"),
    });
    Ok(Json(enriched))
}

pub(super) async fn memory_proxy_expand_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(memory_id): Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
    let memory_id = encode_memoria_memory_id(&memory_id);
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
    let memory_id = encode_memoria_memory_id(&memory_id);
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
    let memory_id = encode_memoria_memory_id(&memory_id);
    memory_proxy_call(
        &state,
        &headers,
        reqwest::Method::POST,
        &format!("/v1/memories/{memory_id}/feedback"),
        body,
    )
    .await
}

pub(super) async fn memory_proxy_delete_by_id_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(memory_id): Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
    let memory_id = encode_memoria_memory_id(&memory_id);
    memory_proxy_call(
        &state,
        &headers,
        reqwest::Method::DELETE,
        &format!("/v1/memories/{memory_id}"),
        serde_json::json!({}),
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
        apply_memoria_management_identity, apply_memory_proxy_identity, encode_memoria_memory_id,
        exact_memory_ids_for_user_purge, is_strict_session_recall, memory_proxy_scope,
        parse_memoria_forward_status, should_inject_memory_proxy_identity,
    };
    use axum::http::StatusCode;
    use serde_json::json;

    #[test]
    fn apply_memory_proxy_identity_overwrites_user_but_preserves_authorized_session() {
        let body = json!({
            "content": "probe",
            "memory_type": "semantic",
            "user_id": "spoofed-user",
            "session_id": "spoofed-session"
        });

        let out = apply_memory_proxy_identity(body, "real-user", true, "/v1/memories");

        assert_eq!(out["user_id"].as_str(), Some("real-user"));
        assert_eq!(out["session_id"].as_str(), Some("spoofed-session"));
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
    fn memory_proxy_scope_keeps_authenticated_owner_and_durable_session_distinct() {
        let scope = memory_proxy_scope(
            &json!({"session_id": "session-7", "user_id": "spoofed"}),
            "user-3",
        )
        .unwrap()
        .unwrap();
        assert_eq!(scope.user_id, "user-3");
        assert_eq!(scope.session_id, "session-7");
        assert!(memory_proxy_scope(&json!({"session_id": 7}), "user-3").is_err());
        assert!(memory_proxy_scope(&json!({"session_id": " session-7"}), "user-3").is_err());
    }

    #[test]
    fn only_retrieve_is_a_strict_session_response_contract() {
        assert!(is_strict_session_recall(
            "/v1/memories/retrieve",
            &json!({"session_id": "session-7", "session_scope": "only"})
        ));
        assert!(!is_strict_session_recall(
            "/v1/memories/retrieve",
            &json!({"session_id": "session-7"})
        ));
        assert!(!is_strict_session_recall(
            "/v1/memories",
            &json!({"session_id": "session-7", "session_scope": "only"})
        ));
    }

    #[test]
    fn memory_id_is_encoded_as_one_upstream_path_segment() {
        assert_eq!(encode_memoria_memory_id("a/b ?"), "a%2Fb%20%3F");
    }

    #[test]
    fn user_purge_accepts_only_bounded_exact_ids() {
        assert_eq!(
            exact_memory_ids_for_user_purge(&json!({
                "memory_ids": ["m1", "m1", "m2"]
            }))
            .unwrap(),
            vec!["m1".to_string(), "m2".to_string()]
        );
        assert!(exact_memory_ids_for_user_purge(&json!({"topic": "shared"})).is_err());
        assert!(exact_memory_ids_for_user_purge(&json!({"memory_ids": []})).is_err());
        assert!(exact_memory_ids_for_user_purge(&json!({"memory_ids": ["m1", 2]})).is_err());
        let too_many = (0..65).map(|index| format!("m{index}")).collect::<Vec<_>>();
        assert!(exact_memory_ids_for_user_purge(&json!({"memory_ids": too_many})).is_err());
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
