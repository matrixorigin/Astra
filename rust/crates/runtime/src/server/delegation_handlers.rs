use super::header_utils::collect_forward_headers;
use super::*;
use astra_services::{DelegationRequest, DelegationResult};

/// POST /chat/runs/{run_id}/delegate
///
/// Delegates a run to one or more sub-agents according to a coordination
/// pattern (fan-out, pipeline, adversarial review, sequential).
pub(super) async fn delegate_run_handler(
    State(state): State<AppState>,
    Path(run_id): Path<String>,
    headers: HeaderMap,
    Json(mut request): Json<DelegationRequest>,
) -> Result<Json<DelegationResponse>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    // Verify the authenticated user owns this run (IDOR prevention).
    state
        .run_lifecycle_service
        .get_run_status(run_id.clone(), user.user_id)
        .await?;
    let forward_headers = collect_forward_headers(&headers);
    request
        .context
        .remove(crate::turn::agentic_delegate_interception::FORWARD_HEADERS_CONTEXT_KEY);

    let engine = state.delegation_engine.as_ref().ok_or_else(|| {
        error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "delegation engine not configured",
        )
    })?;

    // Resolve the source agent identity from the tracker.
    // Top-level runs (not sub-runs) default to "main", which matches
    // the root orchestrator profile registered by register_default_agents().
    let source_agent_id = engine
        .tracker()
        .get_agent_id(&run_id)
        .await
        .unwrap_or_else(|| "main".to_string());

    // Validate the delegation request against the profile registry.
    engine
        .validate(&request, &source_agent_id)
        .await
        .map_err(|e| error_response(StatusCode::BAD_REQUEST, e))?;

    // Execute the delegation.
    let result = engine
        .execute_with_forward_headers(request, &source_agent_id, None, forward_headers, None)
        .await
        .map_err(|e| error_response(StatusCode::INTERNAL_SERVER_ERROR, e))?;

    Ok(Json(DelegationResponse::from(result)))
}

/// GET /chat/runs/{run_id}/delegations
///
/// Returns sub-run IDs spawned by delegations from this parent run.
pub(super) async fn list_delegations_handler(
    State(state): State<AppState>,
    Path(run_id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<DelegationListResponse>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    state
        .run_lifecycle_service
        .get_run_status(run_id.clone(), user.user_id)
        .await?;

    let engine = state.delegation_engine.as_ref().ok_or_else(|| {
        error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "delegation engine not configured",
        )
    })?;

    let sub_runs = engine.tracker().get_children(&run_id).await;
    Ok(Json(DelegationListResponse {
        parent_run_id: run_id,
        sub_run_ids: sub_runs,
    }))
}

// ─── Response types ─────────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
pub(super) struct DelegationResponse {
    pub delegation_id: String,
    pub status: String,
    pub agent_results: Vec<DelegationAgentResult>,
    pub aggregated_output: Option<String>,
    pub total_prompt_tokens: u64,
    pub total_completion_tokens: u64,
    pub total_tool_calls: u32,
}

#[derive(Debug, Serialize)]
pub(super) struct DelegationAgentResult {
    pub agent_id: String,
    pub status: String,
    pub output: Option<String>,
    pub error: Option<String>,
}

impl From<DelegationResult> for DelegationResponse {
    fn from(r: DelegationResult) -> Self {
        Self {
            delegation_id: r.delegation_id,
            status: r.status,
            agent_results: r
                .agent_results
                .into_iter()
                .map(|ar| DelegationAgentResult {
                    agent_id: ar.agent_id,
                    status: ar.status,
                    output: ar.output,
                    error: ar.error,
                })
                .collect(),
            aggregated_output: r.aggregated_output,
            total_prompt_tokens: r.total_prompt_tokens,
            total_completion_tokens: r.total_completion_tokens,
            total_tool_calls: r.total_tool_calls,
        }
    }
}

#[derive(Debug, Serialize)]
pub(super) struct DelegationListResponse {
    pub parent_run_id: String,
    pub sub_run_ids: Vec<String>,
}

// ─── Delegation pause / resume ──────────────────────────────────────────────

#[derive(Debug, Serialize)]
pub(super) struct DelegationMutationResponse {
    pub parent_run_id: String,
    pub affected: usize,
}

/// POST /chat/runs/{run_id}/delegations/pause
///
/// Pause all sub-runs delegated from this parent run.
pub(super) async fn pause_delegations_handler(
    State(state): State<AppState>,
    Path(run_id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<DelegationMutationResponse>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    state
        .run_lifecycle_service
        .get_run_status(run_id.clone(), user.user_id)
        .await?;

    let engine = state.delegation_engine.as_ref().ok_or_else(|| {
        error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "delegation engine not configured",
        )
    })?;

    let affected = engine.pause_children_of(&run_id).await;
    Ok(Json(DelegationMutationResponse {
        parent_run_id: run_id,
        affected,
    }))
}

/// POST /chat/runs/{run_id}/delegations/resume
///
/// Resume all sub-runs delegated from this parent run.
pub(super) async fn resume_delegations_handler(
    State(state): State<AppState>,
    Path(run_id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<DelegationMutationResponse>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    state
        .run_lifecycle_service
        .get_run_status(run_id.clone(), user.user_id)
        .await?;

    let engine = state.delegation_engine.as_ref().ok_or_else(|| {
        error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "delegation engine not configured",
        )
    })?;

    let affected = engine.resume_children_of(&run_id).await;
    Ok(Json(DelegationMutationResponse {
        parent_run_id: run_id,
        affected,
    }))
}

#[cfg(test)]
mod tests {
    /// P0-A: All delegation handlers must verify run ownership via
    /// get_run_status (not just authenticate). No `let _user =` patterns.
    #[test]
    fn delegation_handlers_verify_run_ownership() {
        let source = include_str!("delegation_handlers.rs");
        let test_start = source.find("#[cfg(test)]").unwrap_or(source.len());
        let prod_code = &source[..test_start];

        // Must NOT have discarded user identity
        assert!(
            !prod_code.contains("let _user"),
            "delegation handlers must use user identity, not discard it"
        );

        // All 4 handlers must call get_run_status for ownership verification
        let ownership_checks = prod_code.matches("get_run_status").count();
        assert!(
            ownership_checks >= 4,
            "all 4 delegation handlers must verify run ownership via get_run_status, found {ownership_checks}"
        );
    }
}
