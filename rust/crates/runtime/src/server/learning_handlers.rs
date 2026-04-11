use super::*;

pub(super) async fn learning_health_handler() -> Json<LearningHealthResponse> {
    Json(LearningHealthResponse {
        status: "healthy".to_string(),
        service: "learning".to_string(),
        version: "1.0.0".to_string(),
        timestamp: Utc::now().to_rfc3339(),
    })
}

pub(super) async fn learning_signals_handler(
    headers: HeaderMap,
) -> Result<Json<LearningSignalsResponse>, (StatusCode, Json<ErrorResponse>)> {
    require_bearer_auth(&headers)?;

    Ok(Json(LearningSignalsResponse {
        signal_types: vec![
            "wrong_skill",
            "slow_execution",
            "high_cost",
            "low_satisfaction",
        ],
        descriptions: LearningSignalDescriptions {
            wrong_skill: "Incorrect skill selection",
            slow_execution: "Execution time exceeds threshold",
            high_cost: "Execution cost exceeds budget",
            low_satisfaction: "User satisfaction below threshold",
        },
    }))
}

pub(super) async fn learning_stats_handler(
    headers: HeaderMap,
) -> Result<Json<LearningStatsResponse>, (StatusCode, Json<ErrorResponse>)> {
    require_bearer_auth(&headers)?;

    Ok(Json(LearningStatsResponse {
        total_learnings: 0,
        high_confidence: 0,
        low_confidence: 0,
        avg_confidence: 0.0,
        by_signal_type: serde_json::Map::new(),
        weights: serde_json::Map::new(),
        weights_per_signal: serde_json::Map::new(),
        decay: serde_json::Map::new(),
        total_gates: 0,
        passed_gates: 0,
        failed_gates: 0,
        pass_rate: 0.0,
        avg_improvement_pct: 0.0,
        per_skill: serde_json::Map::new(),
        last_learning_time: None,
    }))
}

pub(super) async fn learning_trigger_handler(
    headers: HeaderMap,
    Json(payload): Json<LearningTriggerRequest>,
) -> Result<Json<LearningTriggerResponse>, (StatusCode, Json<ErrorResponse>)> {
    require_bearer_auth(&headers)?;

    if !(1..=30).contains(&payload.days) {
        return Err((
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(ErrorResponse::new("Invalid request body".to_string())),
        ));
    }

    let _ = payload.force;
    let _ = payload.signal_types;
    let _ = payload.weights;

    Ok(Json(LearningTriggerResponse {
        status: "error",
        learned: 0,
        signals_by_type: None,
        gate_verdict: None,
        improvement_pct: None,
        test_count: None,
        error: Some("Learning pipeline removed in skill system cleanup"),
        message: None,
        model_version: "v1.0",
    }))
}
