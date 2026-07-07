use super::*;

#[derive(Deserialize)]
pub(super) struct ReflectQuery {
    #[serde(default)]
    pub topic: String,
    #[serde(default)]
    pub facet: String,
    #[serde(default)]
    pub depth: String,
    #[serde(default)]
    pub horizon: String,
    #[serde(default)]
    pub source_policy: String,
    #[serde(default)]
    pub include_context: bool,
    #[serde(default = "default_reflect_last_n")]
    pub last_n: i32,
    #[serde(default)]
    pub question: String,
}

fn default_reflect_last_n() -> i32 {
    20
}

pub(super) async fn reflect_session_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(session_id): Path<String>,
    Query(params): Query<ReflectQuery>,
) -> Result<Json<ReflectReport>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    let request = astra_services::reflect::ReflectRequest::from_observation_params_with_source(
        non_empty(&params.topic),
        non_empty(&params.facet),
        non_empty(&params.depth),
        non_empty(&params.horizon),
        non_empty(&params.source_policy),
        params.include_context,
        params.last_n,
        &params.question,
    );
    state
        .reflect_service
        .build_evidence(&user.user_id, &session_id, request)
        .await
        .map(Json)
}

pub(super) async fn decision_trace_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(session_id): Path<String>,
    Query(params): Query<ReflectQuery>,
) -> Result<Json<ReflectReport>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    let request =
        astra_services::reflect::ReflectRequest::decision_trace(params.last_n, &params.question);
    state
        .reflect_service
        .build_evidence(&user.user_id, &session_id, request)
        .await
        .map(Json)
}

fn non_empty(value: &str) -> Option<&str> {
    let trimmed = value.trim();
    (!trimmed.is_empty()).then_some(trimmed)
}
