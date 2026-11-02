//! OAuth connection control stays authenticated by Astra, independently of the
//! linked provider identity. The service never returns renewable credentials.
use crate::AppState;
use astra_core::ErrorResponse;
use astra_services::chatgpt::{CompleteAuthorization, ConnectionOperation, StartAuthorization};
use axum::{
    Json,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
};
use serde_json::Value;

type Response = Result<Json<Value>, (StatusCode, Json<ErrorResponse>)>;

async fn execute(state: AppState, headers: HeaderMap, operation: ConnectionOperation) -> Response {
    let user = state.auth_service.current_user(&headers).await?;
    state
        .model_service
        .model_connection_operation(user.user_id, operation)
        .await
        .map(Json)
}
pub async fn list(State(state): State<AppState>, headers: HeaderMap) -> Response {
    execute(state, headers, ConnectionOperation::List).await
}
pub async fn start(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<StartAuthorization>,
) -> Response {
    execute(state, headers, ConnectionOperation::Start(request)).await
}
pub async fn complete(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Json(request): Json<CompleteAuthorization>,
) -> Response {
    execute(state, headers, ConnectionOperation::Complete(id, request)).await
}
pub async fn cancel(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    execute(state, headers, ConnectionOperation::Cancel(id)).await
}
pub async fn models(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    execute(state, headers, ConnectionOperation::Models(id)).await
}
pub async fn disconnect(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    execute(state, headers, ConnectionOperation::Disconnect(id)).await
}
