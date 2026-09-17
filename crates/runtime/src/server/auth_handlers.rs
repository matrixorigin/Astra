use axum::extract::Extension;
use serde_json::Value;

use super::*;

#[cfg(test)]
fn memory_access_for_scopes(scopes: &[String]) -> Option<&'static str> {
    astra_services::auth::memoria::memory_access_for_scopes(scopes).map(|v| v.as_str())
}

pub(super) async fn auth_methods_handler(State(state): State<AppState>) -> Json<Value> {
    let provider = state.auth_service.memoria_credentials().map(|r| r.provider);
    Json(serde_json::json!({
        "password": true,
        "memoria": provider.and_then(|p| p.web_url.map(|url| serde_json::json!({
            "issuer": p.issuer, "authorization_url": url
        })))
    }))
}

pub(super) async fn auth_memoria_disconnect_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<StatusCode, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    state.auth_service.disconnect_memoria(&user.user_id).await?;
    Ok(StatusCode::NO_CONTENT)
}

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

pub(super) async fn auth_memoria_handler(
    Extension(trace): Extension<RequestTrace>,
    State(state): State<AppState>,
    Json(request): Json<AuthMemoriaRequest>,
) -> Result<Json<AuthMemoriaResponse>, (StatusCode, Json<ErrorResponse>)> {
    let login = state
        .auth_service
        .login_memoria(request.connection_key.trim())
        .await?;
    tracing::info!(target: "astra_runtime::auth", request_id = %trace.request_id, "Memoria login succeeded");
    Ok(Json(AuthMemoriaResponse {
        user_id: login.tokens.user_id,
        access_token: login.tokens.access_token,
        refresh_token: login.tokens.refresh_token,
        token_type: login.tokens.token_type,
        expires_in: login.tokens.expires_in,
        memory_access: login.memory_access.as_str().to_string(),
        granted_scopes: login.granted_scopes,
    }))
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

pub(super) async fn auth_reauthenticate_handler(
    Extension(trace): Extension<RequestTrace>,
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<AuthReauthenticateRequest>,
) -> Result<axum::response::Response, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    let proof = state
        .auth_service
        .reauthenticate(&user.user_id, request.into())
        .await?;
    tracing::info!(
        target: "astra_runtime::auth",
        request_id = %trace.request_id,
        user_id = %user.user_id,
        purpose = proof.purpose.as_str(),
        "reauthentication proof issued"
    );
    Ok(axum::response::IntoResponse::into_response((
        [(axum::http::header::CACHE_CONTROL, "no-store")],
        Json(AuthReauthenticateResponse::from(proof)),
    )))
}

pub(super) async fn auth_reauthentication_options_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<axum::response::Response, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    Ok(axum::response::IntoResponse::into_response((
        [(axum::http::header::CACHE_CONTROL, "no-store")],
        Json(
            state
                .auth_service
                .reauthentication_options(&user.user_id)
                .await?,
        ),
    )))
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
    let requires_write = method != reqwest::Method::GET
        && !endpoint.ends_with("/retrieve")
        && !endpoint.ends_with("/search");
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
    let authority = resolve_memoria_user_authority(state, user_id, requires_write)
        .await
        .map_err(map_memoria_forward_error)?;
    let strict_validation_scope = match strict_recall_scope.as_ref() {
        Some(scope) => Some(
            astra_memoria::MemoryScope::new(authority.owner(), &scope.session_id)
                .map_err(|error| error_response(StatusCode::INTERNAL_SERVER_ERROR, error))?,
        ),
        None => None,
    };
    let body = apply_memory_proxy_identity(body, user_id, endpoint)
        .map_err(|error| error_response(StatusCode::BAD_REQUEST, error))?;
    let strict_recall_limit = strict_recall_scope
        .as_ref()
        .map(|_| strict_session_recall_limit(&body));

    let mut response = forward_memoria_with_authority(state, &authority, method, endpoint, body)
        .await
        .map_err(|error| {
            tracing::warn!(
                target: "astra_runtime::auth",
                endpoint = endpoint,
                error = %error,
                "memory proxy forward failed"
            );
            map_memoria_forward_error(MemoriaForwardError::Backend(error))
        })?;

    if let Some(scope) = strict_validation_scope.as_ref() {
        if let Err(error) = astra_memoria::validate_strict_recall_payload(&response, scope) {
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

        // Semantic retrieval is not the authority for strict session memory:
        // its vector/index path may lag a just-written `working` row. Reconcile
        // every strict session recall from Memoria's owner/session/type list;
        // this keeps read-your-write behavior correct even when an older
        // working row is already present in the semantic result. No local
        // overlay or cross-session recall is introduced. The list is bounded
        // by the caller's top_k and validated against the same owner/session
        // scope before it can reach the client.
        {
            let limit = strict_recall_limit.unwrap_or(10);
            let list_request = serde_json::json!({
                "user_id": user_id,
                "session_id": scope.session_id,
                "memory_type": "working",
                "limit": limit,
            });
            match forward_memoria_with_authority(
                state,
                &authority,
                reqwest::Method::GET,
                "/v1/memories",
                list_request,
            )
            .await
            {
                Ok(working) => {
                    if let Err(error) =
                        astra_memoria::validate_strict_recall_payload(&working, scope)
                    {
                        tracing::error!(
                            target: "astra_runtime::auth",
                            user_id = %scope.user_id,
                            session_id = %scope.session_id,
                            error = %error,
                            "Memoria working-memory reconciliation violated session scope"
                        );
                    } else {
                        astra_memoria::merge_strict_recall_working_memory(
                            &mut response,
                            &working,
                            limit,
                        );
                    }
                }
                Err(error) => {
                    // A degraded list path must not turn a valid semantic
                    // response into a hard failure.  The caller still gets
                    // the original scoped result and an explicit diagnostic.
                    tracing::debug!(
                        target: "astra_runtime::auth",
                        user_id = %scope.user_id,
                        session_id = %scope.session_id,
                        error = %error,
                        "Memoria working-memory reconciliation unavailable"
                    );
                }
            }
        }

        if let Err(error) = astra_memoria::validate_strict_recall_payload(&response, scope) {
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
    }

    Ok(Json(response))
}

enum MemoriaUserAuthority {
    Override {
        owner: String,
    },
    Scoped {
        base_url: String,
        key: String,
        owner: String,
    },
    SelfHosted {
        owner: String,
    },
}

impl MemoriaUserAuthority {
    fn owner(&self) -> &str {
        match self {
            Self::Override { owner } | Self::Scoped { owner, .. } | Self::SelfHosted { owner } => {
                owner
            }
        }
    }
}

#[derive(Debug)]
enum MemoriaForwardError {
    ConsentDenied(String),
    SelfHostedAccessDisabled,
    AccessDisabled,
    Unconfigured,
    Backend(String),
}

impl std::fmt::Display for MemoriaForwardError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ConsentDenied(message) | Self::Backend(message) => f.write_str(message),
            Self::SelfHostedAccessDisabled | Self::AccessDisabled => {
                f.write_str("memory access is not enabled for this Astra account")
            }
            Self::Unconfigured => f.write_str("Memoria credential resolver is not configured"),
        }
    }
}

fn require_memoria_consent(
    access: astra_services::auth::memoria::MemoryAccess,
    requires_write: bool,
) -> Result<(), MemoriaForwardError> {
    match access.denial_message(requires_write) {
        Some(message) => Err(MemoriaForwardError::ConsentDenied(message.into())),
        None => Ok(()),
    }
}

// This classifies an already-denied request for presentation only. Never use
// this classification to grant fallback authority or bypass scoped consent.
fn disabled_memoria_authority_error<T>(
    resolution: &astra_services::auth::memoria::MemoriaCredentialResolution<T>,
    has_login_website: bool,
) -> MemoriaForwardError {
    if !has_login_website
        && matches!(
            resolution,
            astra_services::auth::memoria::MemoriaCredentialResolution::UnboundLocal
        )
    {
        MemoriaForwardError::SelfHostedAccessDisabled
    } else {
        MemoriaForwardError::AccessDisabled
    }
}

async fn resolve_memoria_user_authority(
    state: &AppState,
    user_id: &str,
    requires_write: bool,
) -> Result<MemoriaUserAuthority, MemoriaForwardError> {
    if state.memoria_forwarder_is_override {
        return Ok(MemoriaUserAuthority::Override {
            owner: user_id.to_string(),
        });
    }
    let resolver = state
        .auth_service
        .memoria_credentials()
        .ok_or(MemoriaForwardError::Unconfigured)?;
    let resolution = resolver
        .resolve_runtime(user_id)
        .await
        .map_err(MemoriaForwardError::Backend)?;
    let disabled_error =
        disabled_memoria_authority_error(&resolution, resolver.provider.web_url.is_some());
    match crate::turn::cloud::memoria_compact::select_memoria_authority(
        resolution,
        state.memoria_self_hosted_fallback_enabled,
    ) {
        crate::turn::cloud::memoria_compact::MemoriaAuthoritySelection::Scoped(credential) => {
            require_memoria_consent(credential.access, requires_write)?;
            Ok(MemoriaUserAuthority::Scoped {
                base_url: resolver.provider.base_url,
                key: credential.key,
                owner: credential.owner,
            })
        }
        crate::turn::cloud::memoria_compact::MemoriaAuthoritySelection::SelfHosted => {
            Ok(MemoriaUserAuthority::SelfHosted {
                owner: user_id.to_string(),
            })
        }
        crate::turn::cloud::memoria_compact::MemoriaAuthoritySelection::Disabled => {
            Err(disabled_error)
        }
    }
}

async fn forward_memoria_with_authority(
    state: &AppState,
    authority: &MemoriaUserAuthority,
    method: reqwest::Method,
    endpoint: &str,
    mut body: serde_json::Value,
) -> Result<serde_json::Value, String> {
    if matches!(
        authority,
        MemoriaUserAuthority::Override { .. } | MemoriaUserAuthority::SelfHosted { .. }
    ) {
        let object = body
            .as_object_mut()
            .ok_or("Memoria owner-scoped request requires a JSON object body")?;
        object.insert(
            "user_id".to_string(),
            serde_json::Value::String(authority.owner().to_string()),
        );
        return state
            .memoria_forwarder
            .forward(method, endpoint, body)
            .await;
    }
    let MemoriaUserAuthority::Scoped { base_url, key, .. } = authority else {
        unreachable!("non-scoped authorities return through the configured forwarder")
    };
    if let Some(object) = body.as_object_mut() {
        object.remove("user_id");
    }
    let url = format!("{}{}", base_url.trim_end_matches('/'), endpoint);
    let request = astra_core::net::client_builder_for_target(&url)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|_| "Memoria HTTP client unavailable")?
        .request(method.clone(), url)
        .bearer_auth(key)
        .header("X-Memoria-Tool", "astra")
        .timeout(std::time::Duration::from_secs(30));
    let response = if method == reqwest::Method::GET {
        request.query(&body)
    } else {
        request.json(&body)
    }
    .send()
    .await
    .map_err(|error| format!("Memoria request failed: {error}"))?;
    let status = response.status();
    let text = response
        .text()
        .await
        .map_err(|error| format!("Memoria response read error: {error}"))?;
    if !status.is_success() {
        let bounded: String = text.chars().take(4096).collect();
        return Err(format!("Memoria error {status}: {bounded}"));
    }
    if text.trim().is_empty() {
        return Ok(serde_json::json!({}));
    }
    serde_json::from_str(&text).map_err(|error| format!("Memoria parse error: {error}"))
}

async fn forward_memoria_for_user(
    state: &AppState,
    user_id: &str,
    requires_write: bool,
    method: reqwest::Method,
    endpoint: &str,
    body: serde_json::Value,
) -> Result<serde_json::Value, MemoriaForwardError> {
    let authority = resolve_memoria_user_authority(state, user_id, requires_write).await?;
    forward_memoria_with_authority(state, &authority, method, endpoint, body)
        .await
        .map_err(MemoriaForwardError::Backend)
}

fn map_memoria_forward_error(error: MemoriaForwardError) -> (StatusCode, Json<ErrorResponse>) {
    let error = match error {
        MemoriaForwardError::ConsentDenied(_)
        | MemoriaForwardError::SelfHostedAccessDisabled
        | MemoriaForwardError::AccessDisabled => {
            let code = match &error {
                MemoriaForwardError::ConsentDenied(_) => "memory_consent_denied",
                MemoriaForwardError::SelfHostedAccessDisabled => {
                    "memory_self_hosted_access_disabled"
                }
                _ => "memory_access_disabled",
            };
            return (
                StatusCode::FORBIDDEN,
                Json(ErrorResponse::new(error.to_string()).with_error_code(code)),
            );
        }
        MemoriaForwardError::Unconfigured => {
            return error_response(StatusCode::SERVICE_UNAVAILABLE, error.to_string());
        }
        MemoriaForwardError::Backend(message) => message,
    };
    if error.contains("not configured") {
        error_response(StatusCode::SERVICE_UNAVAILABLE, &error)
    } else if let Some(status) = parse_memoria_forward_status(&error) {
        error_response(status, &error)
    } else {
        internal_error(&error)
    }
}

const MAX_STRICT_SESSION_RECALL_ITEMS: usize = 50;

fn strict_session_recall_limit(body: &serde_json::Value) -> usize {
    body.get("top_k")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(10)
        .clamp(1, MAX_STRICT_SESSION_RECALL_ITEMS as u64) as usize
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

fn normalize_exact_memory_purge_receipt(
    response: serde_json::Value,
    memory_ids: &[String],
) -> Result<serde_json::Value, &'static str> {
    let Some(deleted_count) = response
        .get("purged")
        .or_else(|| response.get("deleted_count"))
        .and_then(serde_json::Value::as_u64)
    else {
        return Err("memory backend did not return a confirmed purge count");
    };
    let requested_count = u64::try_from(memory_ids.len()).unwrap_or(u64::MAX);
    if deleted_count > requested_count {
        return Err("memory backend returned a purge count larger than the exact request");
    }
    let unresolved_count = requested_count.saturating_sub(deleted_count);
    let status = if unresolved_count == 0 {
        "completed"
    } else if deleted_count == 0 {
        "not_found"
    } else {
        "partial"
    };
    let message = match status {
        "completed" => format!(
            "memory_purge: backend confirmed all {deleted_count} exact entries were removed"
        ),
        "not_found" => {
            format!("memory_purge: none of the {requested_count} exact entries matched; 0 removed")
        }
        _ => format!(
            "memory_purge: backend confirmed {deleted_count}/{requested_count} exact entries removed; {unresolved_count} remain unresolved"
        ),
    };
    let identity_resolution = match status {
        "completed" => "all_requested_confirmed",
        "not_found" => "none_requested_confirmed",
        _ => "aggregate_count_only",
    };
    Ok(serde_json::json!({
        "status": status,
        "requested_count": requested_count,
        "deleted_count": deleted_count,
        "unresolved_count": unresolved_count,
        "requested_memory_ids": memory_ids,
        "identity_resolution": identity_resolution,
        "receipt_source": "memoria_purge",
        "message": message,
    }))
}

fn parse_memoria_forward_status(error: &str) -> Option<StatusCode> {
    let suffix = error.strip_prefix("Memoria error ")?;
    let code = suffix.split_whitespace().next()?.parse::<u16>().ok()?;
    StatusCode::from_u16(code).ok()
}

fn encode_memoria_memory_id(memory_id: &str) -> String {
    use percent_encoding::{NON_ALPHANUMERIC, utf8_percent_encode};
    utf8_percent_encode(memory_id, NON_ALPHANUMERIC).to_string()
}

fn apply_memory_proxy_identity(
    mut body: serde_json::Value,
    user_id: &str,
    endpoint: &str,
) -> Result<serde_json::Value, &'static str> {
    let obj = body
        .as_object_mut()
        .ok_or("memory request body must be a JSON object")?;
    // Authentication owns `user_id`; the durable session id remains a
    // separate, caller-selected identity that was authorized against the
    // session store before this function runs.
    obj.insert(
        "user_id".to_string(),
        serde_json::Value::String(user_id.to_string()),
    );

    // An exact-ID purge and a session purge are mutually exclusive selectors.
    // Keep the authenticated user identity: the HTTP forwarder projects it to
    // Memoria's X-User-Id scope header before serializing the request body.
    if endpoint.ends_with("/purge")
        && let Some(obj) = body.as_object_mut()
    {
        obj.remove("session_id");
    }

    Ok(body)
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
    let reason = body
        .get("reason")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|reason| !reason.is_empty())
        .ok_or_else(|| {
            error_response(
                StatusCode::BAD_REQUEST,
                "memory purge requires a non-empty reason",
            )
        })?;
    let response = memory_proxy_call_for_user(
        &state,
        &user.user_id,
        reqwest::Method::POST,
        "/v1/memories/purge",
        serde_json::json!({
            "memory_ids": memory_ids.clone(),
            "reason": reason,
        }),
    )
    .await?
    .0;
    let receipt = normalize_exact_memory_purge_receipt(response, &memory_ids)
        .map_err(|error| error_response(StatusCode::BAD_GATEWAY, error))?;
    Ok(Json(receipt))
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
    let requires_write = method != reqwest::Method::GET;
    forward_memoria_for_user(state, &user.user_id, requires_write, method, endpoint, body)
        .await
        .map(Json)
        .map_err(|error| {
            tracing::warn!(
                target: "astra_runtime::auth",
                endpoint = endpoint,
                error = %error,
                "memoria management proxy forward failed"
            );
            map_memoria_forward_error(error)
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
    let name = encode_memoria_memory_id(&name);
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
    let name = encode_memoria_memory_id(&name);
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
    let name = encode_memoria_memory_id(&name);
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
    let name = encode_memoria_memory_id(&name);
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
    let name = encode_memoria_memory_id(&name);
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
    use super::{MemoriaForwardError, map_memoria_forward_error, require_memoria_consent};
    use super::{
        apply_memoria_management_identity, apply_memory_proxy_identity, encode_memoria_memory_id,
        exact_memory_ids_for_user_purge, is_strict_session_recall, memory_access_for_scopes,
        memory_proxy_scope, normalize_exact_memory_purge_receipt, parse_memoria_forward_status,
    };
    use axum::http::StatusCode;
    use serde_json::json;

    #[test]
    fn memoria_consent_denials_map_to_forbidden_independently_of_guidance() {
        use astra_services::auth::memoria::MemoryAccess;
        for (access, write) in [
            (MemoryAccess::None, false),
            (MemoryAccess::None, true),
            (MemoryAccess::ReadOnly, true),
        ] {
            let error = require_memoria_consent(access, write).unwrap_err();
            let (status, body) = map_memoria_forward_error(error);
            assert_eq!(status, StatusCode::FORBIDDEN);
            assert_eq!(body.detail, access.denial_message(write).unwrap());
            assert_eq!(body.error_code.as_deref(), Some("memory_consent_denied"));
        }
        let (status, body) = map_memoria_forward_error(MemoriaForwardError::ConsentDenied(
            "A completely different localized message".into(),
        ));
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(body.detail, "A completely different localized message");
        assert_eq!(body.error_code.as_deref(), Some("memory_consent_denied"));
        assert!(require_memoria_consent(MemoryAccess::ReadOnly, false).is_ok());
        assert!(require_memoria_consent(MemoryAccess::ReadWrite, true).is_ok());
    }

    #[test]
    fn memoria_disabled_codes_preserve_local_hosted_and_revoked_distinctions() {
        use astra_services::auth::memoria::MemoriaCredentialResolution;
        for (resolution, has_website, code) in [
            (
                MemoriaCredentialResolution::<()>::UnboundLocal,
                false,
                "memory_self_hosted_access_disabled",
            ),
            (
                MemoriaCredentialResolution::UnboundLocal,
                true,
                "memory_access_disabled",
            ),
            (
                MemoriaCredentialResolution::Denied,
                false,
                "memory_access_disabled",
            ),
            (
                MemoriaCredentialResolution::Denied,
                true,
                "memory_access_disabled",
            ),
        ] {
            let error = super::disabled_memoria_authority_error(&resolution, has_website);
            let (status, body) = map_memoria_forward_error(error);
            assert_eq!(status, StatusCode::FORBIDDEN);
            assert_eq!(body.error_code.as_deref(), Some(code));
            assert_eq!(
                body.detail,
                "memory access is not enabled for this Astra account"
            );
        }
    }

    #[test]
    fn memoria_forward_mapping_preserves_configuration_and_backend_statuses() {
        assert_eq!(
            map_memoria_forward_error(MemoriaForwardError::Unconfigured).0,
            StatusCode::SERVICE_UNAVAILABLE
        );
        assert_eq!(
            map_memoria_forward_error(MemoriaForwardError::Backend(
                "Memoria error 401 Unauthorized: revoked".into()
            ))
            .0,
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            map_memoria_forward_error(MemoriaForwardError::Backend(
                "unexpected backend failure".into()
            ))
            .0,
            StatusCode::INTERNAL_SERVER_ERROR
        );
    }

    #[test]
    fn memoria_scope_sets_map_only_to_supported_access_modes() {
        let strings = |values: &[&str]| {
            values
                .iter()
                .map(|value| value.to_string())
                .collect::<Vec<_>>()
        };
        assert_eq!(
            memory_access_for_scopes(&strings(&["identity:read"])),
            Some("none")
        );
        assert_eq!(
            memory_access_for_scopes(&strings(&["memory:read", "identity:read"])),
            Some("read_only")
        );
        assert_eq!(
            memory_access_for_scopes(&strings(&["memory:write", "identity:read", "memory:read",])),
            Some("read_write")
        );
        assert_eq!(memory_access_for_scopes(&strings(&["memory:read"])), None);
        assert_eq!(
            memory_access_for_scopes(&strings(&["identity:read", "admin"])),
            None
        );
    }

    #[test]
    fn apply_memory_proxy_identity_overwrites_user_but_preserves_authorized_session() {
        let body = json!({
            "content": "probe",
            "memory_type": "semantic",
            "user_id": "spoofed-user",
            "session_id": "spoofed-session"
        });

        let out = apply_memory_proxy_identity(body, "real-user", "/v1/memories").unwrap();

        assert_eq!(out["user_id"].as_str(), Some("real-user"));
        assert_eq!(out["session_id"].as_str(), Some("spoofed-session"));
    }

    #[test]
    fn apply_memory_proxy_identity_keeps_authenticated_owner_for_purge() {
        let body = json!({
            "memory_ids": ["m1"],
            "user_id": "spoofed-user",
            "session_id": "spoofed-session"
        });

        let out = apply_memory_proxy_identity(body, "real-user", "/v1/memories/purge").unwrap();

        assert_eq!(out["user_id"], "real-user");
        assert!(out.get("session_id").is_none());
        assert_eq!(out["memory_ids"], json!(["m1"]));
    }

    #[test]
    fn apply_memory_proxy_identity_rejects_non_object_body() {
        assert_eq!(
            apply_memory_proxy_identity(json!([]), "real-user", "/v1/memories").unwrap_err(),
            "memory request body must be a JSON object"
        );
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
    fn strict_recall_reconciliation_appends_only_bounded_working_items() {
        let mut response = json!({
            "items": [{
                "memory_id": "existing",
                "memory_type": "episodic",
                "user_id": "user-3",
                "session_id": "session-7"
            }]
        });
        let working = json!({
            "items": [
                {"memory_id": "existing", "memory_type": "working", "user_id": "user-3", "session_id": "session-7"},
                {"memory_id": "working-1", "memory_type": "working", "user_id": "user-3", "session_id": "session-7"},
                {"memory_id": "not-working", "memory_type": "episodic", "user_id": "user-3", "session_id": "session-7"},
                {"memory_id": "working-2", "memory_type": "working", "user_id": "user-3", "session_id": "session-7"}
            ]
        });

        assert!(astra_memoria::merge_strict_recall_working_memory(
            &mut response,
            &working,
            3
        ));
        let items = response["items"].as_array().unwrap();
        assert_eq!(items.len(), 3);
        assert_eq!(items[0]["memory_id"], "working-1");
        assert_eq!(items[1]["memory_id"], "working-2");
        assert_eq!(items[2]["memory_id"], "existing");
        assert!(!items.iter().any(|item| item["memory_id"] == "not-working"));
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
    fn exact_purge_receipt_uses_backend_count_instead_of_requested_count() {
        let ids = vec!["m1".to_string(), "m2".to_string(), "m3".to_string()];
        let receipt = normalize_exact_memory_purge_receipt(json!({"purged": 2}), &ids).unwrap();

        assert_eq!(receipt["status"], "partial");
        assert_eq!(receipt["requested_count"], 3);
        assert_eq!(receipt["deleted_count"], 2);
        assert_eq!(receipt["unresolved_count"], 1);
        assert_eq!(receipt["identity_resolution"], "aggregate_count_only");
        assert_eq!(receipt["requested_memory_ids"], json!(ids));
        assert!(receipt.get("memory_ids").is_none());
        assert_eq!(receipt["receipt_source"], "memoria_purge");
    }

    #[test]
    fn exact_purge_receipt_never_fabricates_success_for_noop_or_ambiguous_response() {
        let ids = vec!["missing".to_string()];
        let noop = normalize_exact_memory_purge_receipt(json!({"deleted_count": 0}), &ids).unwrap();
        assert_eq!(noop["status"], "not_found");
        assert_eq!(noop["deleted_count"], 0);
        assert_eq!(noop["identity_resolution"], "none_requested_confirmed");

        assert!(normalize_exact_memory_purge_receipt(json!({}), &ids).is_err());
        assert!(normalize_exact_memory_purge_receipt(json!({"purged": 2}), &ids).is_err());
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
