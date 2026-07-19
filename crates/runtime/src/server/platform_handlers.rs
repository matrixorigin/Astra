use super::*;
use astra_services::agents::AgentListResponse;
use astra_services::events::EventListResponse;

/// Aggregated platform snapshot returned by `GET /platform/snapshot`.
#[derive(Serialize)]
pub(super) struct PlatformSnapshotResponse {
    health: HealthResponse,
    agents: AgentListResponse,
    sessions: SessionListResponse,
    events: EventListResponse,
    timestamp: String,
}

/// Returns an aggregated snapshot of the platform: health, agents, sessions,
/// and recent events in a single round-trip.
pub(super) async fn platform_snapshot_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<PlatformSnapshotResponse>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;

    // Fan out all reads concurrently.
    let (health, agents_res, sessions_res, events_res) = tokio::join!(
        super::meta_handlers::current_health(&state),
        state.agent_service.list_agents(user.user_id.clone()),
        state.session_service.list_sessions(SessionListFilter {
            user_id: user.user_id.clone(),
            agent_id: None,
            status: None,
            limit: 50,
            cursor: None,
        }),
        state.event_service.list_events(EventListFilter {
            user_id: user.user_id,
            session_id: None,
            event_type: None,
            agent_id: None,
            causal_chain_id: None,
            limit: 50,
            cursor: None,
        }),
    );

    Ok(Json(PlatformSnapshotResponse {
        health,
        agents: AgentListResponse::from(agents_res?),
        sessions: SessionListResponse::from(sessions_res?),
        events: EventListResponse::from(events_res?),
        timestamp: Utc::now().to_rfc3339(),
    }))
}
