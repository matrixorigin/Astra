//! Phase 13 — Webhook alert channel.
//!
//! Dispatches [`TraceAlert`]s at or above a configured severity threshold to
//! an external HTTP endpoint. Failures are swallowed (logged via `tracing`)
//! so a flaky webhook never crashes the pipeline.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::Duration;

use crate::trace_alert::{AlertSeverity, TraceAlert};

/// Stable JSON payload sent per alert.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WebhookPayload {
    pub session_id: String,
    pub rule: String,
    pub severity: String,
    pub turn: u32,
    pub message: String,
}

impl WebhookPayload {
    pub fn from_alert(session_id: &str, a: &TraceAlert) -> Self {
        Self {
            session_id: session_id.to_string(),
            rule: a.rule.clone(),
            severity: match a.severity {
                AlertSeverity::Info => "info",
                AlertSeverity::Warning => "warning",
                AlertSeverity::Error => "error",
            }
            .to_string(),
            turn: a.turn,
            message: a.message.clone(),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum WebhookError {
    #[error("transport error: {0}")]
    Transport(String),
    #[error("non-success status: {0}")]
    Status(u16),
}

/// Abstraction over the HTTP client so tests can inject a capturing stub.
#[async_trait]
pub trait WebhookClient: Send + Sync {
    async fn post(&self, url: &str, payload: &WebhookPayload) -> Result<(), WebhookError>;
}

/// Default [`WebhookClient`] using `reqwest`.
pub struct ReqwestWebhookClient {
    client: reqwest::Client,
}

impl ReqwestWebhookClient {
    pub fn new() -> Self {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        Self { client }
    }
}

impl Default for ReqwestWebhookClient {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl WebhookClient for ReqwestWebhookClient {
    async fn post(&self, url: &str, payload: &WebhookPayload) -> Result<(), WebhookError> {
        let resp = self
            .client
            .post(url)
            .json(payload)
            .send()
            .await
            .map_err(|e| WebhookError::Transport(e.to_string()))?;
        let status = resp.status();
        if !status.is_success() {
            return Err(WebhookError::Status(status.as_u16()));
        }
        Ok(())
    }
}

/// Runtime configuration for the alert webhook channel.
#[derive(Debug, Clone)]
pub struct AlertWebhookConfig {
    pub url: String,
    pub min_severity: AlertSeverity,
}

/// Dispatches alerts to a configured webhook endpoint.
pub struct AlertDispatcher {
    cfg: AlertWebhookConfig,
    client: Arc<dyn WebhookClient>,
}

impl AlertDispatcher {
    pub fn new(cfg: AlertWebhookConfig, client: Arc<dyn WebhookClient>) -> Self {
        Self { cfg, client }
    }

    /// Dispatch all alerts meeting the severity threshold.
    ///
    /// Errors are swallowed — alerting must never crash the pipeline.
    pub async fn dispatch(&self, session_id: &str, alerts: &[TraceAlert]) {
        for alert in alerts {
            if alert.severity < self.cfg.min_severity {
                continue;
            }
            let payload = WebhookPayload::from_alert(session_id, alert);
            if let Err(e) = self.client.post(&self.cfg.url, &payload).await {
                tracing::warn!(
                    target: "trace_alert",
                    rule = %alert.rule,
                    session_id = %session_id,
                    error = %e,
                    "alert webhook dispatch failed"
                );
            }
        }
    }
}
