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
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<LearningSignalsResponse>, (StatusCode, Json<ErrorResponse>)> {
    let _user = state.auth_service.current_user(&headers).await?;

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
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<LearningStatsResponse>, (StatusCode, Json<ErrorResponse>)> {
    let _user = state.auth_service.current_user(&headers).await?;

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
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(payload): Json<LearningTriggerRequest>,
) -> Result<Json<LearningTriggerResponse>, (StatusCode, Json<ErrorResponse>)> {
    let _user = state.auth_service.current_user(&headers).await?;

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

#[cfg(test)]
mod tests {
    /// P0-D: Learning handlers must use current_user (JWT validation),
    /// not require_bearer_auth (header-only check).
    #[test]
    fn learning_handlers_use_current_user() {
        let source = include_str!("learning_handlers.rs");
        let test_start = source.find("#[cfg(test)]").unwrap_or(source.len());
        let prod_code = &source[..test_start];
        assert!(
            !prod_code.contains("require_bearer_auth"),
            "learning handlers must not use require_bearer_auth (no JWT validation)"
        );
        let current_user_count = prod_code.matches("current_user").count();
        assert!(
            current_user_count >= 3,
            "all 3 authenticated learning handlers must use current_user, found {current_user_count}"
        );
    }
}
