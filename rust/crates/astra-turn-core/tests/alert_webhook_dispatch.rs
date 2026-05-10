//! Phase 13 — Webhook alert channel TDD tests.
//!
//! Verifies that alerts above a configured severity threshold are dispatched
//! to an HTTP webhook with a stable JSON payload shape.

use astra_turn_core::alert_dispatcher::{
    AlertDispatcher, AlertWebhookConfig, WebhookClient, WebhookError, WebhookPayload,
};
use astra_turn_core::trace_alert::{AlertSeverity, TraceAlert};
use std::sync::{Arc, Mutex};

/// Capturing test client — records every dispatch for assertions.
#[derive(Default, Clone)]
struct CapturingClient {
    sent: Arc<Mutex<Vec<WebhookPayload>>>,
    fail_next: Arc<Mutex<bool>>,
}

#[async_trait::async_trait]
impl WebhookClient for CapturingClient {
    async fn post(&self, _url: &str, payload: &WebhookPayload) -> Result<(), WebhookError> {
        if *self.fail_next.lock().unwrap() {
            *self.fail_next.lock().unwrap() = false;
            return Err(WebhookError::Transport("simulated".into()));
        }
        self.sent.lock().unwrap().push(payload.clone());
        Ok(())
    }
}

fn alert(rule: &str, sev: AlertSeverity) -> TraceAlert {
    TraceAlert {
        severity: sev,
        rule: rule.to_string(),
        message: format!("{rule} fired"),
        turn: 1,
    }
}

#[tokio::test]
async fn dispatcher_sends_error_alerts_when_threshold_is_warning() {
    let client = CapturingClient::default();
    let cfg = AlertWebhookConfig {
        url: "https://example.invalid/hook".into(),
        min_severity: AlertSeverity::Warning,
    };
    let dispatcher = AlertDispatcher::new(cfg, Arc::new(client.clone()));
    dispatcher
        .dispatch("sess-1", &[alert("recovery_loop", AlertSeverity::Error)])
        .await;
    let sent = client.sent.lock().unwrap();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].rule, "recovery_loop");
    assert_eq!(sent[0].session_id, "sess-1");
    assert_eq!(sent[0].severity, "error");
}

#[tokio::test]
async fn dispatcher_filters_below_threshold() {
    let client = CapturingClient::default();
    let cfg = AlertWebhookConfig {
        url: "https://example.invalid/hook".into(),
        min_severity: AlertSeverity::Error,
    };
    let dispatcher = AlertDispatcher::new(cfg, Arc::new(client.clone()));
    dispatcher
        .dispatch(
            "sess-1",
            &[
                alert("cache_cold_start", AlertSeverity::Warning),
                alert("cache_regression", AlertSeverity::Warning),
            ],
        )
        .await;
    assert!(client.sent.lock().unwrap().is_empty());
}

#[tokio::test]
async fn dispatcher_forwards_multiple_alerts_independently() {
    let client = CapturingClient::default();
    let cfg = AlertWebhookConfig {
        url: "https://example.invalid/hook".into(),
        min_severity: AlertSeverity::Warning,
    };
    let dispatcher = AlertDispatcher::new(cfg, Arc::new(client.clone()));
    dispatcher
        .dispatch(
            "sess-42",
            &[
                alert("compaction_cascade", AlertSeverity::Warning),
                alert("recovery_loop", AlertSeverity::Error),
            ],
        )
        .await;
    let sent = client.sent.lock().unwrap();
    assert_eq!(sent.len(), 2);
    let rules: Vec<&str> = sent.iter().map(|p| p.rule.as_str()).collect();
    assert!(rules.contains(&"compaction_cascade"));
    assert!(rules.contains(&"recovery_loop"));
}

#[tokio::test]
async fn dispatcher_swallows_transport_errors_without_panic() {
    let client = CapturingClient::default();
    *client.fail_next.lock().unwrap() = true;
    let cfg = AlertWebhookConfig {
        url: "https://example.invalid/hook".into(),
        min_severity: AlertSeverity::Warning,
    };
    let dispatcher = AlertDispatcher::new(cfg, Arc::new(client.clone()));
    // Must not panic — failed webhook shouldn't crash the pipeline.
    dispatcher
        .dispatch("sess-x", &[alert("x", AlertSeverity::Error)])
        .await;
    assert!(client.sent.lock().unwrap().is_empty());
}

#[test]
fn payload_shape_is_stable_json() {
    let a = alert("recovery_loop", AlertSeverity::Error);
    let p = WebhookPayload::from_alert("sess-9", &a);
    let v = serde_json::to_value(&p).unwrap();
    assert_eq!(v["session_id"], "sess-9");
    assert_eq!(v["rule"], "recovery_loop");
    assert_eq!(v["severity"], "error");
    assert_eq!(v["turn"], 1);
    assert!(v["message"].as_str().unwrap().contains("recovery_loop"));
}
