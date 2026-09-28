//! Operator-controlled deployment boundary. All paths use existing admin auth;
//! none trusts a saved offline approval or reads server-local evidence files.
use super::*;
use astra_services::runs::RunStateStore;
use astra_services::tuning::rollout::*;
use astra_turn_core::model_routing::rollout::{
    RouterPublishRequest, deployment_candidate, prepare_shadow,
};
use serde::Deserialize;
type ApiError = (StatusCode, Json<ErrorResponse>);
fn store(state: &AppState) -> Result<DatabaseRouterRolloutStore, ApiError> {
    state
        .shared_pool
        .clone()
        .map(DatabaseRouterRolloutStore)
        .ok_or_else(|| {
            error_response(
                StatusCode::SERVICE_UNAVAILABLE,
                "Router deployment requires durable storage",
            )
        })
}
fn bad(error: impl ToString) -> ApiError {
    error_response(StatusCode::BAD_REQUEST, error.to_string())
}
fn conflict(error: impl ToString) -> ApiError {
    error_response(StatusCode::CONFLICT, error.to_string())
}

pub(super) async fn publish(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(owner): Path<String>,
    Json(request): Json<RouterPublishRequest>,
) -> Result<Json<RouterRolloutState>, ApiError> {
    let admin = state.admin.authorizer.require_admin(&headers).await?;
    let expected = request.expected_revision;
    let owner_for_job = owner.clone();
    // CPU-only qualification over the explicitly uploaded reviewed corpus.
    let d =
        tokio::task::spawn_blocking(move || prepare_shadow(request, &owner_for_job, Utc::now()))
            .await
            .map_err(internal_error)?
            .map_err(bad)?;
    let candidate = deployment_candidate(&d).map_err(bad)?;
    let policy = astra_config::RuntimeConfig::cached()
        .model_routing
        .as_ref()
        .ok_or_else(|| bad("Server Auto policy is disabled"))?;
    if d.policy_revision != policy.revision
        || candidate.economy.offering_id != policy.economy_offering_id
        || candidate.strong.offering_id != policy.strong_offering_id
    {
        return Err(bad("Deployment differs from Server Auto policy"));
    }
    for profile in [&candidate.economy, &candidate.strong] {
        let execution = state
            .model_service
            .admit_model_offering(owner.clone(), profile.offering_id.clone())
            .await?;
        if model_execution_admission::model_execution_contract_root(&execution)
            != profile.contract_root
        {
            return Err(bad(
                "Qualified model contract differs from current Offering",
            ));
        }
    }
    Ok(Json(
        store(&state)?
            .change(
                &owner,
                expected,
                &admin.user_id,
                RolloutChange::Publish(Box::new(d)),
            )
            .await
            .map_err(conflict)?,
    ))
}

pub(super) async fn status(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(owner): Path<String>,
) -> Result<Json<RouterRolloutDashboard>, ApiError> {
    state.admin.authorizer.require_admin(&headers).await?;
    let store = store(&state)?;
    let current = store.load(&owner).await.map_err(internal_error)?;
    let (runs, truncated) = store.runs(&owner, &current).await.map_err(internal_error)?;
    Ok(Json(dashboard(&current, &runs, truncated)))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct CanaryRequest {
    expected_revision: u64,
    basis_points: u16,
}
pub(super) async fn canary(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(owner): Path<String>,
    Json(request): Json<CanaryRequest>,
) -> Result<Json<RouterRolloutState>, ApiError> {
    let admin = state.admin.authorizer.require_admin(&headers).await?;
    let store = store(&state)?;
    let current = store.load(&owner).await.map_err(internal_error)?;
    if current.revision != request.expected_revision {
        return Err(conflict("Rollout revision conflict"));
    }
    let d = current
        .deployment
        .as_ref()
        .ok_or_else(|| bad("No deployment"))?;
    let (runs, truncated) = store.runs(&owner, &current).await.map_err(internal_error)?;
    let report = dashboard(&current, &runs, truncated);
    let shadow = report
        .cohorts
        .get("shadow")
        .ok_or_else(|| bad("Live shadow observations required"))?;
    if truncated
        || shadow.sessions < d.review.minimum_shadow_sessions
        || shadow.admission_failures != 0
        || shadow.critical_violations != 0
        || shadow
            .p95_routing_overhead_us
            .is_none_or(|us| us > d.review.maximum_routing_overhead_ms * 1000)
    {
        return Err(bad(
            "Live shadow support, admission or overhead gate failed",
        ));
    }
    Ok(Json(
        store
            .change(
                &owner,
                request.expected_revision,
                &admin.user_id,
                RolloutChange::Canary {
                    basis_points: request.basis_points,
                },
            )
            .await
            .map_err(conflict)?,
    ))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RollbackRequest {
    expected_revision: u64,
    reason: String,
}
pub(super) async fn rollback(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(owner): Path<String>,
    Json(request): Json<RollbackRequest>,
) -> Result<Json<RouterRolloutState>, ApiError> {
    let admin = state.admin.authorizer.require_admin(&headers).await?;
    Ok(Json(
        store(&state)?
            .change(
                &owner,
                request.expected_revision,
                &admin.user_id,
                RolloutChange::Rollback {
                    reason: request.reason,
                },
            )
            .await
            .map_err(conflict)?,
    ))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RevokeRequest {
    expected_revision: u64,
    source_ids: Vec<String>,
}
pub(super) async fn revoke(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(owner): Path<String>,
    Json(request): Json<RevokeRequest>,
) -> Result<Json<RouterRolloutState>, ApiError> {
    let admin = state.admin.authorizer.require_admin(&headers).await?;
    Ok(Json(
        store(&state)?
            .change(
                &owner,
                request.expected_revision,
                &admin.user_id,
                RolloutChange::Revoke {
                    source_ids: request.source_ids,
                },
            )
            .await
            .map_err(conflict)?,
    ))
}

pub(super) async fn outcome(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((owner, run_id)): Path<(String, String)>,
    Json(outcome): Json<RouterReviewedOutcome>,
) -> Result<StatusCode, ApiError> {
    let admin = state.admin.authorizer.require_admin(&headers).await?;
    outcome.validate().map_err(bad)?;
    let registry = store(&state)?;
    let runs = astra_services::runs::DatabaseRunStateStore::new(registry.0.clone());
    let run = runs
        .load_run(&owner, &run_id)
        .await
        .map_err(internal_error)?
        .ok_or_else(|| bad("Unknown owner/run"))?;
    if !matches!(run.status.as_str(), "completed" | "failed" | "cancelled") {
        return Err(bad("Outcome requires a terminal run"));
    }
    let event = runs
        .load_run_event_by_idempotency_key(
            &owner,
            &run_id,
            astra_services::model_routing::EVENT_TYPE,
            astra_services::model_routing::DECISION_KEY,
        )
        .await
        .map_err(internal_error)?
        .ok_or_else(|| bad("Run has no routing decision"))?;
    let decision: astra_services::model_routing::ModelRoutingDecision =
        serde_json::from_value(event["data"].clone()).map_err(bad)?;
    let pinned = decision
        .rollout
        .as_ref()
        .ok_or_else(|| bad("Run has no rollout"))?;
    if outcome.rubric_version != pinned.rubric_version {
        return Err(bad("Outcome rubric differs from the pinned deployment"));
    }
    // Historical reports remain valid. Only stop the exact deployment that
    // produced the run; publication may race this check but cannot be stopped.
    if outcome.critical_violation {
        let current = registry.load(&owner).await.map_err(internal_error)?;
        if current.deployment.as_ref().is_some_and(|d| {
            d.deployment_id == pinned.deployment_id && d.mode != RolloutMode::RolledBack
        }) && let Err(error) = registry
            .change(
                &owner,
                current.revision,
                &admin.user_id,
                RolloutChange::CriticalViolation {
                    deployment_id: pinned.deployment_id.clone(),
                },
            )
            .await
        {
            let latest = registry.load(&owner).await.map_err(internal_error)?;
            if latest
                .deployment
                .as_ref()
                .is_some_and(|d| d.deployment_id == pinned.deployment_id)
            {
                return Err(conflict(error));
            }
        }
    }
    let record = ReviewedOutcomeRecord {
        deployment_id: pinned.deployment_id.clone(),
        reviewed_by: admin.user_id,
        reviewed_at: Utc::now(),
        outcome,
    };
    let event =
        serde_json::json!({"event_type":OUTCOME_EVENT,"idempotency_key":OUTCOME_KEY,"data":record});
    // Timestamp is server-owned; retry the same reviewed payload idempotently.
    if let Some(saved) = runs
        .load_run_event_by_idempotency_key(&owner, &run_id, OUTCOME_EVENT, OUTCOME_KEY)
        .await
        .map_err(internal_error)?
    {
        if saved["data"]["outcome"] == event["data"]["outcome"] {
            return Ok(StatusCode::OK);
        }
        return Err(conflict("A different reviewed outcome is already recorded"));
    }
    let written = runs
        .append_events_if_current_generation_and_status(
            &owner,
            &run.session_id,
            &run_id,
            run.run_generation,
            &[run.status.as_str()],
            &[event],
        )
        .await
        .map_err(conflict)?;
    if !written {
        return Err(conflict("Run changed during outcome recording"));
    }
    Ok(StatusCode::CREATED)
}
