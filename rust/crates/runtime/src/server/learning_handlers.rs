use super::*;

pub(super) async fn learning_health_handler() -> Json<LearningHealthResponse> {
    // Lessons are now stored in Memoria (Session Memory Protocol L3),
    // not in the agent_lessons DB table. Health is reported by Memoria's
    // own /health endpoint. This handler returns a static healthy status.
    Json(LearningHealthResponse {
        status: "healthy".to_string(),
        service: "learning".to_string(),
        version: "1.0.0".to_string(),
        timestamp: Utc::now().to_rfc3339(),
        lesson_count: None,
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
    use super::*;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    use astra_core::{ErrorResponse, error_response};
    use astra_services::{
        AuthRefreshRequestData, AuthRegisterRequestData, AuthService, AuthTokenRecord,
        AuthUserRecord,
    };
    use async_trait::async_trait;
    use axum::{
        Json,
        extract::State,
        http::{HeaderMap, HeaderValue, StatusCode},
    };

    use crate::{AppState, HealthChecker, ServiceInfo};

    #[derive(Clone)]
    struct AlwaysHealthy;

    #[async_trait]
    impl HealthChecker for AlwaysHealthy {
        async fn database_healthy(&self) -> bool {
            true
        }
    }

    #[derive(Default)]
    struct RecordingAuthService {
        current_user_calls: AtomicUsize,
    }

    #[async_trait]
    impl AuthService for RecordingAuthService {
        async fn register(
            &self,
            _request: AuthRegisterRequestData,
        ) -> Result<AuthUserRecord, (StatusCode, Json<ErrorResponse>)> {
            unreachable!("register is not used in learning handler tests")
        }

        async fn login(
            &self,
            _request: astra_services::AuthLoginRequestData,
        ) -> Result<AuthTokenRecord, (StatusCode, Json<ErrorResponse>)> {
            unreachable!("login is not used in learning handler tests")
        }

        async fn refresh(
            &self,
            _request: AuthRefreshRequestData,
        ) -> Result<AuthTokenRecord, (StatusCode, Json<ErrorResponse>)> {
            unreachable!("refresh is not used in learning handler tests")
        }

        async fn logout(
            &self,
            _request: AuthRefreshRequestData,
        ) -> Result<(), (StatusCode, Json<ErrorResponse>)> {
            unreachable!("logout is not used in learning handler tests")
        }

        async fn current_user(
            &self,
            headers: &HeaderMap,
        ) -> Result<AuthUserRecord, (StatusCode, Json<ErrorResponse>)> {
            self.current_user_calls.fetch_add(1, Ordering::SeqCst);
            if headers.get("authorization").is_none() {
                return Err(error_response(
                    StatusCode::UNAUTHORIZED,
                    "missing authorization",
                ));
            }
            Ok(AuthUserRecord {
                user_id: "learning-user".into(),
                username: "learning-user".into(),
                email: "learning@example.com".into(),
                display_name: None,
            })
        }
    }

    fn build_state(auth_service: Arc<dyn AuthService>) -> AppState {
        AppState::new(ServiceInfo::default(), Arc::new(AlwaysHealthy))
            .with_auth_service(auth_service)
    }

    fn auth_headers() -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            "authorization",
            HeaderValue::from_static("Bearer learning-test-token"),
        );
        headers
    }

    /// P0-D: Authenticated learning handlers must consult AuthService::current_user
    /// before returning data or trigger results.
    #[tokio::test]
    async fn authenticated_learning_handlers_use_current_user() {
        let auth = Arc::new(RecordingAuthService::default());
        let state = build_state(auth.clone());

        let Json(signals) = learning_signals_handler(State(state.clone()), auth_headers())
            .await
            .expect("signals handler should succeed");
        assert_eq!(signals.signal_types.len(), 4);

        let Json(stats) = learning_stats_handler(State(state.clone()), auth_headers())
            .await
            .expect("stats handler should succeed");
        assert_eq!(stats.total_learnings, 0);

        let Json(trigger) = learning_trigger_handler(
            State(state),
            auth_headers(),
            Json(LearningTriggerRequest {
                days: 7,
                force: false,
                signal_types: Vec::new(),
                weights: None,
            }),
        )
        .await
        .expect("trigger handler should succeed");
        assert_eq!(trigger.status, "error");

        assert_eq!(
            auth.current_user_calls.load(Ordering::SeqCst),
            3,
            "all authenticated learning handlers should call current_user once"
        );
    }
}
