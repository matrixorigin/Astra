use super::*;

#[derive(Deserialize)]
pub(super) struct ReflectQuery {
    #[serde(default = "default_reflect_focus")]
    pub focus: String,
    #[serde(default = "default_reflect_last_n")]
    pub last_n: i32,
    #[serde(default)]
    pub question: String,
}

fn default_reflect_focus() -> String {
    "auto".into()
}
fn default_reflect_last_n() -> i32 {
    20
}

pub(super) async fn reflect_session_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(session_id): Path<String>,
    Query(params): Query<ReflectQuery>,
) -> Result<Json<ReflectEvidence>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    state
        .reflect_service
        .build_evidence(
            &user.user_id,
            &session_id,
            &params.focus,
            params.last_n,
            &params.question,
        )
        .await
        .map(Json)
}

pub(super) async fn decision_trace_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(session_id): Path<String>,
    Query(params): Query<ReflectQuery>,
) -> Result<Json<ReflectEvidence>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    state
        .reflect_service
        .build_evidence(
            &user.user_id,
            &session_id,
            "tool_selection",
            params.last_n,
            &params.question,
        )
        .await
        .map(Json)
}

#[derive(Deserialize)]
pub(super) struct LearningFeedbackRequest {
    pub event_id: String,
    pub satisfaction_score: Option<i64>,
}

#[derive(Serialize)]
pub(super) struct LearningFeedbackResponse {
    pub status: String,
    pub message: String,
}

pub(super) async fn learning_feedback_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<LearningFeedbackRequest>,
) -> Result<Json<LearningFeedbackResponse>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    let record = state
        .learning_feedback_service
        .submit_feedback(LearningFeedbackRequestData {
            event_id: request.event_id,
            user_id: user.user_id,
            satisfaction_score: request.satisfaction_score,
        })
        .await?;
    Ok(Json(LearningFeedbackResponse {
        status: record.status,
        message: record.message,
    }))
}
