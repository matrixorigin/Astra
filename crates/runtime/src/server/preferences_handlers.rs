//! User preferences REST surface.
//!
//! Edge-cloud contract: the CLI must not connect to MatrixOne
//! directly for preference sync. These endpoints wrap the
//! existing `MatrixOneSyncService` so edge clients pull/push
//! preferences over HTTP, with the user resolved from the auth
//! header (no client-supplied user_id to forge).
//!
//! Endpoints:
//! - `GET /preferences` — pull all preferences for the authed user.
//! - `PUT /preferences/{key}` — push a single preference value.
//!
use super::*;
use astra_services::state_sync::MatrixOneSyncService;

#[derive(Serialize)]
pub(super) struct PreferencesResponse {
    pub preferences: Vec<PreferenceEntry>,
}

#[derive(Serialize)]
pub(super) struct PreferenceEntry {
    pub key: String,
    pub value: String,
}

#[derive(Deserialize)]
pub(super) struct PutPreferenceRequest {
    pub value: String,
}

/// `GET /preferences` — pull every preference for the authed user.
pub(super) async fn list_preferences_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<PreferencesResponse>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    let Some(pool) = state.shared_pool.as_ref() else {
        return Err(error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "preferences store not configured on this server",
        ));
    };
    let svc = MatrixOneSyncService::new(pool.get().clone());
    let prefs = svc
        .pull_all_preferences(&user.user_id)
        .await
        .map_err(|e| error_response(StatusCode::INTERNAL_SERVER_ERROR, e));
    let prefs = prefs?;
    Ok(Json(PreferencesResponse {
        preferences: prefs
            .into_iter()
            .map(|(k, v)| PreferenceEntry { key: k, value: v })
            .collect(),
    }))
}

/// `PUT /preferences/{key}` — push a single preference value.
/// Returns 204 No Content on success.
pub(super) async fn put_preference_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(key): Path<String>,
    Json(req): Json<PutPreferenceRequest>,
) -> Result<StatusCode, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    let Some(pool) = state.shared_pool.as_ref() else {
        return Err(error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "preferences store not configured on this server",
        ));
    };
    let svc = MatrixOneSyncService::new(pool.get().clone());
    svc.push_preference(&user.user_id, &key, &req.value)
        .await
        .map_err(|e| error_response(StatusCode::INTERNAL_SERVER_ERROR, e))?;
    Ok(StatusCode::NO_CONTENT)
}
