//! Product harness HTTP handlers.
//!
//! These endpoints are distinct from `/sessions/{session_id}/harness/*`, which
//! exposes diagnostic harness snapshots for agent-loop observability.

use super::*;

pub async fn list_harness_templates_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Vec<HarnessTemplateRecord>>, (StatusCode, Json<ErrorResponse>)> {
    state.auth_service.current_user(&headers).await?;
    state.harness_service.list_templates().await.map(Json)
}

pub async fn list_harness_node_catalog_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Vec<HarnessNodeCatalogRecord>>, (StatusCode, Json<ErrorResponse>)> {
    state.auth_service.current_user(&headers).await?;
    state.harness_service.list_node_catalog().await.map(Json)
}

pub async fn create_skillify_harness_run_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<SkillifyRunRequest>,
) -> Result<(StatusCode, Json<HarnessRunRecord>), (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    state
        .harness_service
        .create_skillify_run(user.user_id, request)
        .await
        .map(|run| (StatusCode::CREATED, Json(run)))
}

pub async fn get_harness_run_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(harness_run_id): Path<String>,
) -> Result<Json<HarnessRunRecord>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    state
        .harness_service
        .get_run(user.user_id, harness_run_id)
        .await
        .map(Json)
}

pub async fn list_harness_run_items_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(harness_run_id): Path<String>,
) -> Result<Json<Vec<HarnessItemRecord>>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    state
        .harness_service
        .list_run_items(user.user_id, harness_run_id)
        .await
        .map(Json)
}

pub async fn decide_harness_item_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((harness_run_id, item_id)): Path<(String, String)>,
    Json(request): Json<HarnessDecisionRequest>,
) -> Result<Json<HarnessItemRecord>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    state
        .harness_service
        .decide_item(user.user_id, harness_run_id, item_id, request)
        .await
        .map(Json)
}

pub async fn list_skill_drafts_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(harness_run_id): Path<String>,
) -> Result<Json<Vec<HarnessSkillDraftRecord>>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    state
        .harness_service
        .list_skill_drafts(user.user_id, harness_run_id)
        .await
        .map(Json)
}

pub async fn get_skill_draft_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((harness_run_id, skill_draft_id)): Path<(String, String)>,
) -> Result<Json<HarnessSkillDraftRecord>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    state
        .harness_service
        .get_skill_draft(user.user_id, harness_run_id, skill_draft_id)
        .await
        .map(Json)
}

pub async fn decide_skill_draft_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((harness_run_id, skill_draft_id)): Path<(String, String)>,
    Json(request): Json<HarnessDecisionRequest>,
) -> Result<Json<HarnessSkillDraftRecord>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    state
        .harness_service
        .decide_skill_draft(user.user_id, harness_run_id, skill_draft_id, request)
        .await
        .map(Json)
}

pub async fn decide_skill_rule_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((harness_run_id, skill_draft_id, skill_rule_id)): Path<(String, String, String)>,
    Json(request): Json<HarnessDecisionRequest>,
) -> Result<Json<HarnessSkillDraftRecord>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    state
        .harness_service
        .decide_skill_rule(
            user.user_id,
            harness_run_id,
            skill_draft_id,
            skill_rule_id,
            request,
        )
        .await
        .map(Json)
}

pub async fn publish_skill_draft_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((harness_run_id, skill_draft_id)): Path<(String, String)>,
    Json(request): Json<SkillifyPublishRequest>,
) -> Result<(StatusCode, Json<SkillifyPublishRecord>), (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    state
        .harness_service
        .publish_skill_draft(user.user_id, harness_run_id, skill_draft_id, request)
        .await
        .map(|record| (StatusCode::CREATED, Json(record)))
}

pub async fn create_skillify_draft_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(harness_run_id): Path<String>,
    Json(request): Json<SkillifyDraftRequest>,
) -> Result<(StatusCode, Json<SkillifyDraftRecord>), (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    state
        .harness_service
        .create_skillify_draft(user.user_id, harness_run_id, request)
        .await
        .map(|draft| (StatusCode::CREATED, Json(draft)))
}
