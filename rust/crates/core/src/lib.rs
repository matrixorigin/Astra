use std::sync::Arc;

use axum::{
    Json,
    http::{HeaderMap, StatusCode},
};
use serde::Serialize;
use sqlx::{MySql, Pool, mysql::MySqlPoolOptions};

pub mod config;
pub mod runtime_limits;
pub use config::*;
pub use runtime_limits::{RuntimeLimits, DEV_MATRIXONE_PASSWORD, warn_default_credentials_once};
pub use sqlx;

/// Create a one-shot connection pool (legacy — prefer `SharedPool` for production).
pub async fn connect_matrixone(settings: &MatrixOneSettings) -> Result<Pool<MySql>, sqlx::Error> {
    MySqlPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(std::time::Duration::from_secs(2))
        .connect(&settings.database_url())
        .await
}

/// Shared connection pool that can be cloned cheaply across services.
#[derive(Clone, Debug)]
pub struct SharedPool {
    pool: Arc<Pool<MySql>>,
    settings: MatrixOneSettings,
}

impl SharedPool {
    pub async fn new(settings: &MatrixOneSettings) -> Result<Self, sqlx::Error> {
        let pool = MySqlPoolOptions::new()
            .max_connections(10)
            .min_connections(1)
            .acquire_timeout(std::time::Duration::from_secs(5))
            .idle_timeout(std::time::Duration::from_secs(300))
            .connect(&settings.database_url())
            .await?;
        Ok(Self {
            pool: Arc::new(pool),
            settings: settings.clone(),
        })
    }

    pub fn get(&self) -> &Pool<MySql> {
        &self.pool
    }

    pub fn settings(&self) -> &MatrixOneSettings {
        &self.settings
    }

    pub async fn close(&self) {
        self.pool.close().await;
    }
}

#[derive(Serialize, PartialEq, Eq)]
pub struct ErrorResponse {
    pub detail: String,
}

pub fn error_response(
    status: StatusCode,
    detail: impl Into<String>,
) -> (StatusCode, Json<ErrorResponse>) {
    (
        status,
        Json(ErrorResponse {
            detail: detail.into(),
        }),
    )
}

pub fn internal_error(error: impl ToString) -> (StatusCode, Json<ErrorResponse>) {
    error_response(StatusCode::INTERNAL_SERVER_ERROR, error.to_string())
}

pub fn current_unix_seconds() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs_f64())
        .unwrap_or(0.0)
}

pub fn bearer_token(headers: &HeaderMap) -> Result<&str, (StatusCode, Json<ErrorResponse>)> {
    headers
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        .filter(|value| value.starts_with("Bearer "))
        .map(|value| &value["Bearer ".len()..])
        .filter(|value| !value.is_empty())
        .ok_or_else(|| error_response(StatusCode::UNAUTHORIZED, "Not authenticated"))
}
