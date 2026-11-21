use std::sync::{Arc, Mutex};

use astra_core::{MatrixOneSettings, SharedPool};
use astra_runtime::{
    AppState, HealthChecker, ServiceInfo, build_app, server::run::engine::RunEngine,
};
use astra_services::{
    auth::{StubAuthService, session::DatabaseSessionService},
    runs::DatabaseRunStateStore,
};
use async_trait::async_trait;
use axum::{
    body::{self, Body},
    http::{Request, StatusCode},
};
use tower::util::ServiceExt;
use tracing::instrument::WithSubscriber;
use tracing_subscriber::prelude::*;
use uuid::Uuid;

struct Healthy;

#[async_trait]
impl HealthChecker for Healthy {
    async fn database_healthy(&self) -> bool {
        true
    }
}

#[derive(Clone, Default)]
struct Statements(Arc<Mutex<Vec<String>>>);

impl tracing::field::Visit for Statements {
    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        if field.name() == "db.statement" {
            self.0.lock().unwrap().push(value.to_owned());
        }
    }
    fn record_debug(&mut self, _: &tracing::field::Field, _: &dyn std::fmt::Debug) {}
}
impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for Statements {
    fn on_event(&self, event: &tracing::Event<'_>, _: tracing_subscriber::layer::Context<'_, S>) {
        if event.metadata().target() == "sqlx::query" {
            event.record(&mut self.clone());
        }
    }
}

#[tokio::test]
#[ignore = "requires isolated live MatrixOne"]
async fn owned_run_tree_http_reads_session_and_tree() {
    assert_eq!(std::env::var("ASTRA_TEST_DB_IT").as_deref(), Ok("1"));
    let settings = MatrixOneSettings::from_env();
    astra_services::ensure_core_schema(&settings, "mysql")
        .await
        .unwrap();
    let pool = SharedPool::new(&settings).await.unwrap();
    let session = Uuid::new_v4().to_string();
    sqlx::query("INSERT INTO agent_sessions (user_id, session_id, status) VALUES ('test-user', ?, 'active')")
        .bind(&session).execute(pool.get()).await.unwrap();
    let foreign_session = Uuid::new_v4().to_string();
    sqlx::query("INSERT INTO agent_sessions (user_id, session_id, status) VALUES ('foreign-user', ?, 'active')")
        .bind(&foreign_session).execute(pool.get()).await.unwrap();
    let store = Arc::new(DatabaseRunStateStore::new(pool.clone()));
    let lifecycle = astra_runtime::AgenticRunLifecycleService::new(
        settings.clone(),
        Arc::new(astra_runtime::FernetTokenEncryptor::new("0123456789abcdef").unwrap()),
        Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new())),
        RunEngine::new(store),
    )
    .with_pool(pool.clone());
    let state = AppState::new(ServiceInfo::default(), Arc::new(Healthy))
        .with_shared_pool(pool.clone())
        .with_auth_service(Arc::new(StubAuthService))
        .with_session_service(Arc::new(
            DatabaseSessionService::new(settings).with_pool(pool.clone()),
        ))
        .with_run_lifecycle_service(Arc::new(lifecycle));
    let app = build_app(state);
    let statements = Statements::default();
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/sessions/{session}/runs?limit=200"))
                .header("authorization", "Bearer test")
                .body(Body::empty())
                .unwrap(),
        )
        .with_subscriber(tracing_subscriber::registry().with(statements.clone()))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(value["session_id"], session);
    assert_eq!(value["runs"].as_array().unwrap().len(), 0);
    let queries = statements.0.lock().unwrap().clone();
    println!("business queries: {queries:#?}");
    assert_eq!(
        queries
            .iter()
            .filter(|s| s.trim_start().starts_with("SELECT"))
            .count(),
        2
    );
    let session_query = queries
        .iter()
        .find(|sql| sql.contains("FROM agent_sessions"))
        .expect("owned Session lookup");
    assert!(!session_query.contains("session_context_heads"));
    assert!(!session_query.contains("metadata"));
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/sessions/{}/runs", Uuid::new_v4()))
                .header("authorization", "Bearer test")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let denied_statements = Statements::default();
    let response = app
        .oneshot(
            Request::builder()
                .uri(format!("/sessions/{foreign_session}/runs"))
                .header("authorization", "Bearer test")
                .body(Body::empty())
                .unwrap(),
        )
        .with_subscriber(tracing_subscriber::registry().with(denied_statements.clone()))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let denied_queries = denied_statements.0.lock().unwrap().clone();
    assert!(
        denied_queries
            .iter()
            .any(|sql| sql.contains("FROM agent_sessions"))
    );
    assert!(denied_queries.iter().all(|sql| !sql.contains("agent_runs")));
    sqlx::query("DELETE FROM agent_sessions WHERE user_id = ? AND session_id = ?")
        .bind("test-user")
        .bind(&session)
        .execute(pool.get())
        .await
        .unwrap();
    sqlx::query("DELETE FROM agent_sessions WHERE user_id = ? AND session_id = ?")
        .bind("foreign-user")
        .bind(&foreign_session)
        .execute(pool.get())
        .await
        .unwrap();
}
