use astra_services::context::*;

use crate::AppState;
use astra_core::ErrorResponse;
use axum::{
    Json,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
};

pub async fn create_snapshot_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<SnapshotCreateRequest>,
) -> Result<(StatusCode, Json<SnapshotResponse>), (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    let snap = state
        .context_service
        .create_snapshot(
            user.user_id,
            SnapshotCreateRequestData {
                session_id: request.session_id,
                event_id: request.event_id,
                context_data: request.context_data,
            },
        )
        .await?;
    Ok((StatusCode::CREATED, Json(SnapshotResponse::from(snap))))
}

pub async fn list_snapshots_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<SnapshotListQuery>,
) -> Result<Json<SnapshotListResponse>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    let list = state
        .context_service
        .list_snapshots(SnapshotListFilter {
            user_id: user.user_id,
            session_id: q.session_id,
            limit: q.limit,
            offset: q.offset,
        })
        .await?;
    Ok(Json(SnapshotListResponse::from(list)))
}

pub async fn get_snapshot_handler(
    State(state): State<AppState>,
    Path(context_capture_id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<SnapshotResponse>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    let snap = state
        .context_service
        .get_snapshot(context_capture_id, user.user_id)
        .await?;
    Ok(Json(SnapshotResponse::from(snap)))
}
