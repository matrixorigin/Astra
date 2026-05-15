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
//! Implementation note: the underlying `StateSyncService` trait has
//! many other methods (plans-pack, tasks-pack, etc.) used by
//! server-internal sync flows. Those are NOT exposed here — this
//! file only covers the two methods CLI calls today, keeping the
//! attack surface tight.

use super::*;
use astra_services::state_sync::{MatrixOneSyncService, StateSyncService};

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

/// Build an ephemeral `MatrixOneSyncService` for one request.
/// Audit flusher is spawned per-request and drained at handler
/// exit so a single push/pull always lands its audit row before
/// the response returns. A long-lived flusher would be lower
/// overhead but couples request lifetime with a background task,
/// which is overkill for the low call rate of preference sync.
fn build_sync_service(
    pool: &astra_core::SharedPool,
) -> (
    MatrixOneSyncService,
    astra_services::state_sync::AuditFlusherHandle,
) {
    let raw = pool.get().clone();
    let flusher = astra_services::state_sync::spawn_audit_flusher(raw.clone());
    let svc = MatrixOneSyncService::new(raw, flusher.writer.clone());
    (svc, flusher)
}

async fn drain_flusher(
    svc: MatrixOneSyncService,
    flusher: astra_services::state_sync::AuditFlusherHandle,
) {
    drop(svc);
    drop(flusher.writer);
    flusher.shutdown.cancel();
    if let Err(e) = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        flusher.join_handle,
    )
    .await
    {
        tracing::warn!(
            "preferences audit flusher drain timed out (5s): {e}; some entries may be lost"
        );
    }
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
    let (svc, flusher) = build_sync_service(pool);
    let prefs = svc
        .pull_all_preferences(&user.user_id)
        .await
        .map_err(|e| error_response(StatusCode::INTERNAL_SERVER_ERROR, e));
    drain_flusher(svc, flusher).await;
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
    let (svc, flusher) = build_sync_service(pool);
    let result = svc.push_preference(&user.user_id, &key, &req.value).await;
    drain_flusher(svc, flusher).await;
    if !result.success {
        return Err(error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            if result.message.is_empty() {
                "push failed".to_string()
            } else {
                result.message
            },
        ));
    }
    Ok(StatusCode::NO_CONTENT)
}
