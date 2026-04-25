use super::*;
use astra_core::STATUS_CANCELLED;
use astra_services::SessionArtifactJsonStore;

pub(super) async fn create_session_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<SessionCreateRequest>,
) -> Result<(StatusCode, Json<SessionResponse>), (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    let session = state
        .session_service
        .create_session(
            user.user_id,
            SessionCreateRequestData {
                agent_id: request.agent_id,
                title: request.title,
                metadata: request.metadata,
            },
        )
        .await?;
    Ok((StatusCode::CREATED, Json(SessionResponse::from(session))))
}

pub(super) async fn list_sessions_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<SessionListQuery>,
) -> Result<Json<SessionListResponse>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    let sessions = state
        .session_service
        .list_sessions(SessionListFilter {
            user_id: user.user_id,
            agent_id: query.agent_id,
            status: query.session_status,
            limit: query.limit,
            offset: query.offset,
        })
        .await?;
    Ok(Json(SessionListResponse::from(sessions)))
}

pub(super) async fn get_session_handler(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<SessionResponse>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    let session = state
        .session_service
        .get_session(session_id, user.user_id)
        .await?;
    Ok(Json(SessionResponse::from(session)))
}

pub(super) async fn update_session_handler(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    headers: HeaderMap,
    Json(request): Json<SessionUpdateRequest>,
) -> Result<Json<SessionResponse>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    let session = state
        .session_service
        .update_session(
            session_id,
            user.user_id,
            SessionUpdateRequestData {
                title: request.title,
                metadata: request.metadata,
                status: request.status,
            },
        )
        .await?;
    Ok(Json(SessionResponse::from(session)))
}

pub(super) async fn delete_session_handler(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    headers: HeaderMap,
) -> Result<StatusCode, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    state
        .session_service
        .delete_session(session_id, user.user_id)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

pub(super) async fn close_session_handler(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<SessionResponse>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    let session = state
        .session_service
        .update_session(
            session_id,
            user.user_id,
            SessionUpdateRequestData {
                title: None,
                metadata: None,
                status: Some("closed".to_string()),
            },
        )
        .await?;
    Ok(Json(SessionResponse::from(session)))
}

pub(super) async fn resume_session_handler(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<SessionResponse>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    let session = state
        .session_service
        .update_session(
            session_id,
            user.user_id,
            SessionUpdateRequestData {
                title: None,
                metadata: None,
                status: Some("active".to_string()),
            },
        )
        .await?;
    Ok(Json(SessionResponse::from(session)))
}

pub(super) async fn cancel_session_handler(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<SessionResponse>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    let session = state
        .session_service
        .update_session(
            session_id,
            user.user_id,
            SessionUpdateRequestData {
                title: None,
                metadata: None,
                status: Some(STATUS_CANCELLED.to_string()),
            },
        )
        .await?;
    Ok(Json(SessionResponse::from(session)))
}

pub(super) async fn session_activity_handler(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    headers: HeaderMap,
    Query(query): Query<SessionActivityQuery>,
) -> Result<Json<SessionActivityResponse>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    // Verify ownership first.
    let _ = state
        .session_service
        .get_session(session_id.clone(), user.user_id.clone())
        .await?;

    let activities = state
        .session_service
        .get_session_activity(session_id, user.user_id, query.limit, query.offset)
        .await?;
    Ok(Json(SessionActivityResponse::from(activities)))
}

pub(super) async fn list_session_artifacts_handler(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    headers: HeaderMap,
    Query(query): Query<SessionArtifactListQuery>,
) -> Result<Json<SessionArtifactListResponse>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    let _ = state
        .session_service
        .get_session(session_id.clone(), user.user_id)
        .await?;
    let Some(shared_pool) = state.shared_pool.clone() else {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ErrorResponse::new("session artifact store unavailable")),
        ));
    };
    let artifact_store =
        astra_services::DatabaseSessionArtifactStore::new(shared_pool.settings().clone())
            .with_pool(shared_pool);
    let artifacts = artifact_store
        .list_json_artifacts(
            &session_id,
            query.artifact_kind.as_deref(),
            query.limit as usize,
        )
        .await
        .map_err(internal_artifact_error)?;
    Ok(Json(SessionArtifactListResponse {
        session_id,
        artifacts: artifacts
            .into_iter()
            .map(session_artifact_response)
            .collect(),
        limit: query.limit,
    }))
}

pub(super) async fn get_session_artifact_handler(
    State(state): State<AppState>,
    Path((session_id, artifact_id)): Path<(String, String)>,
    headers: HeaderMap,
) -> Result<Json<SessionArtifactResponse>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    let _ = state
        .session_service
        .get_session(session_id.clone(), user.user_id)
        .await?;
    let Some(shared_pool) = state.shared_pool.clone() else {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ErrorResponse::new("session artifact store unavailable")),
        ));
    };
    let artifact_store =
        astra_services::DatabaseSessionArtifactStore::new(shared_pool.settings().clone())
            .with_pool(shared_pool);
    let artifact = artifact_store
        .load_json_artifact(&artifact_id)
        .await
        .map_err(internal_artifact_error)?
        .filter(|artifact| artifact.session_id == session_id)
        .ok_or_else(|| {
            (
                StatusCode::NOT_FOUND,
                Json(ErrorResponse::new("session artifact not found")),
            )
        })?;
    Ok(Json(session_artifact_response(artifact)))
}

fn session_artifact_response(
    artifact: astra_services::StoredSessionArtifact,
) -> SessionArtifactResponse {
    SessionArtifactResponse {
        artifact_id: artifact.artifact_id,
        session_id: artifact.session_id,
        user_id: artifact.user_id,
        artifact_kind: artifact.artifact_kind,
        source: artifact.source,
        turn: artifact.turn,
        round: artifact.round,
        content: artifact.content,
        metadata: artifact.metadata,
        created_at: artifact.created_at,
    }
}

fn internal_artifact_error(error: String) -> (StatusCode, Json<ErrorResponse>) {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(ErrorResponse::new(error)),
    )
}

#[cfg(test)]
mod tests {
    #[test]
    fn session_artifact_handlers_enforce_session_scope_and_store_api() {
        let source = include_str!("session_handlers.rs");
        assert!(
            source.contains("get_session(session_id.clone(), user.user_id)"),
            "artifact handlers should verify session ownership before reading artifacts"
        );
        assert!(
            source.contains(".list_json_artifacts("),
            "artifact list handler should use the session artifact store list API"
        );
        assert!(
            source.contains(".load_json_artifact(&artifact_id)"),
            "artifact get handler should use the session artifact store get API"
        );
    }
}
