use crate::server::*;

/// Create an `agent_sessions` row while enforcing the per-user session quota.
///
/// The resource governor has two related but distinct responsibilities:
///
/// - session-create quota: counts durable chat sessions (`agent_sessions`)
/// - run-start quota: limits execution capacity for active runs
///
/// Keep the session counter here, immediately around the actual session insert.
/// A single durable session can contain many agentic runs, so run lifecycle code
/// must not increment `sessions_created`.
pub(crate) async fn create_session_with_resource_quota(
    state: &AppState,
    user_id: String,
    request: SessionCreateRequestData,
) -> Result<SessionRecord, (StatusCode, Json<ErrorResponse>)> {
    if let astra_services::resource_governor::LimitCheck::Denied { limit, reason } =
        state.resource_governor.check_session_create(&user_id).await
    {
        return Err(error_response_coded(
            StatusCode::TOO_MANY_REQUESTS,
            format!(
                "Per-user session quota exceeded ({}): {reason}",
                limit.as_str()
            ),
            limit.error_code(),
        ));
    }

    let session = state
        .session_service
        .create_session(user_id.clone(), request)
        .await?;
    state
        .resource_governor
        .record_session_created(&user_id)
        .await;
    Ok(session)
}
