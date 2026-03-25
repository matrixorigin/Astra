pub use mo_agent_services::branches::*;

use crate::AppState;
use axum::{
    Json,
    extract::State,
    http::{HeaderMap, StatusCode},
};
use mo_agent_core::ErrorResponse;

pub async fn create_branch_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<CreateBranchRequest>,
) -> Result<(StatusCode, Json<CreateBranchResponse>), (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    let branch = state
        .branch_service
        .create_branch(
            user.user_id,
            CreateBranchData {
                name: request.name,
                source: request.source,
                snapshot: request.snapshot,
                is_database: request.is_database,
            },
        )
        .await?;
    Ok((StatusCode::CREATED, Json(branch)))
}

pub async fn diff_branch_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<DiffRequest>,
) -> Result<Json<DiffResponse>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    let diff = state
        .branch_service
        .diff_branch(
            user.user_id,
            DiffData {
                target: request.target,
                source: request.source,
                target_snapshot: request.target_snapshot,
                source_snapshot: request.source_snapshot,
                output: request.output,
            },
        )
        .await?;
    Ok(Json(diff))
}

pub async fn merge_branch_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<MergeRequest>,
) -> Result<Json<MergeResponse>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    let merge = state
        .branch_service
        .merge_branch(
            user.user_id,
            MergeData {
                source: request.source,
                target: request.target,
                on_conflict: request.on_conflict,
            },
        )
        .await?;
    Ok(Json(merge))
}

pub async fn delete_branch_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<DeleteBranchRequest>,
) -> Result<Json<StatusResponse>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    let result = state
        .branch_service
        .delete_branch(
            user.user_id,
            DeleteBranchData {
                name: request.name,
                is_database: request.is_database,
            },
        )
        .await?;
    Ok(Json(result))
}

pub async fn estimate_cost_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<CostEstimateRequest>,
) -> Result<Json<CostEstimateResponse>, (StatusCode, Json<ErrorResponse>)> {
    let _user = state.auth_service.current_user(&headers).await?;
    let estimate = state
        .branch_service
        .estimate_cost(CostEstimateData {
            operation: request.operation,
            model: request.model,
            session_count: request.session_count,
            budget_remaining: request.budget_remaining,
        })
        .await?;
    Ok(Json(estimate))
}
