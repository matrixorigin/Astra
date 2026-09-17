//! Run admission control, quota gating, and post-loop memory cleanup permits.
//!
//! This module handles:
//! - Run admission timeouts and capacity errors
//! - Per-user run quota enforcement
//! - Post-loop memory cleanup concurrency control
//! - Metrics registration for admission and cleanup subsystems

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use axum::Json;
use axum::http::StatusCode;
use serde_json::{Value, json};

use astra_core::{ErrorResponse, error_response_coded};

use super::run_state::{
    DurableRunEventBatchBudget, durable_event_type, durable_run_event_estimated_bytes,
};

pub(super) const DEFAULT_RUN_ADMISSION_TIMEOUT_SECS: u64 = 30;
pub(super) const METRIC_RUN_ADMISSION_ATTEMPTS_TOTAL: &str = "astra_run_admission_attempts_total";
pub(super) const METRIC_RUN_ADMISSION_WAIT_MS_TOTAL: &str = "astra_run_admission_wait_ms_total";
pub(super) const METRIC_RUN_ADMISSION_WEIGHT_UNITS_TOTAL: &str =
    "astra_run_admission_weight_units_total";
pub(super) const METRIC_DURABLE_RUN_EVENT_BATCHES_TOTAL: &str =
    "astra_durable_run_event_batches_total";
pub(super) const METRIC_DURABLE_RUN_EVENT_ROWS_TOTAL: &str = "astra_durable_run_event_rows_total";
pub(super) const METRIC_DURABLE_RUN_EVENT_BYTES_TOTAL: &str = "astra_durable_run_event_bytes_total";
pub(super) const METRIC_DURABLE_RUN_EVENT_ROW_BUDGET: &str = "astra_durable_run_event_row_budget";
pub(super) const METRIC_DURABLE_RUN_EVENT_BYTE_BUDGET: &str = "astra_durable_run_event_byte_budget";
pub(super) const METRIC_POST_LOOP_MEMORY_CLEANUP_DISPATCHES_TOTAL: &str =
    "astra_post_loop_memory_cleanup_dispatches_total";
pub(super) const METRIC_POST_LOOP_MEMORY_CLEANUP_WORKERS_TOTAL: &str =
    "astra_post_loop_memory_cleanup_workers_total";
pub(super) const METRIC_SESSION_MEMORY_POST_LOOP_DRAINS_TOTAL: &str =
    "astra_session_memory_post_loop_drains_total";
pub(super) const DEFAULT_POST_LOOP_MEMORY_CLEANUP_CONCURRENCY: usize = 4;
/// The extraction worker has one bounded end-to-end deadline, including
/// provider selection and durable snapshot I/O. Post-loop settlement runs
/// outside the response stream, but must still outlive that contract instead
/// of calling an in-flight memory operation failed or settled prematurely.
pub(super) const DEFAULT_SESSION_MEMORY_POST_LOOP_DRAIN_TIMEOUT_MS: u64 = 45_000;
/// Phase-0 observed behavior: run admission is count-based and every run
/// consumes one unit regardless of prompt size.
pub(super) const CURRENT_RUN_ADMISSION_WEIGHT_UNITS: u64 = 1;
static POST_LOOP_MEMORY_CLEANUP_PERMITS: OnceLock<Arc<tokio::sync::Semaphore>> = OnceLock::new();

pub(super) fn run_admission_timeout() -> Duration {
    Duration::from_secs(DEFAULT_RUN_ADMISSION_TIMEOUT_SECS)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RunAdmissionError {
    Timeout,
    Closed,
    /// The caller cancelled while it was waiting for capacity.  This is not a
    /// capacity failure: the durable cancel transition remains authoritative.
    Cancelled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PreSpawnFailureCode {
    RunAdmissionTimeout,
    RunAdmissionClosed,
    PreSpawnFailure,
}

impl PreSpawnFailureCode {
    pub(super) const fn as_str(self) -> &'static str {
        match self {
            Self::RunAdmissionTimeout => "run_admission_timeout",
            Self::RunAdmissionClosed => "run_admission_closed",
            Self::PreSpawnFailure => "pre_spawn_failure",
        }
    }
}

impl From<RunAdmissionError> for PreSpawnFailureCode {
    fn from(value: RunAdmissionError) -> Self {
        match value {
            RunAdmissionError::Timeout => Self::RunAdmissionTimeout,
            RunAdmissionError::Closed => Self::RunAdmissionClosed,
            // A cancellation is normally handled without a failure
            // transition.  Keep this conversion total so synchronous callers
            // cannot accidentally panic if they opt into cancellable wait.
            RunAdmissionError::Cancelled => Self::PreSpawnFailure,
        }
    }
}

pub(super) fn post_loop_memory_cleanup_permits() -> Arc<tokio::sync::Semaphore> {
    Arc::clone(POST_LOOP_MEMORY_CLEANUP_PERMITS.get_or_init(|| {
        Arc::new(tokio::sync::Semaphore::new(
            DEFAULT_POST_LOOP_MEMORY_CLEANUP_CONCURRENCY,
        ))
    }))
}

pub(super) fn pre_spawn_failure_terminal_events(
    message: &str,
    failure_code: PreSpawnFailureCode,
) -> [Value; 2] {
    let error_code = failure_code.as_str();
    [
        json!({
            "event_type": "run_error",
            "data": {
                "error": message,
                "error_code": error_code,
                "error_kind": "server_error",
            },
        }),
        json!({
            "event_type": "run_finished",
            "data": {
                "total_prompt_tokens": 0,
                "total_completion_tokens": 0,
                "error": message,
                "error_code": error_code,
                "error_kind": "server_error",
            },
        }),
    ]
}

pub(super) fn per_user_run_quota_response(
    limit: astra_services::resource_governor::ResourceLimitKind,
    reason: String,
) -> (StatusCode, Json<ErrorResponse>) {
    error_response_coded(
        StatusCode::TOO_MANY_REQUESTS,
        format!("Per-user run quota exceeded ({}): {reason}", limit.as_str()),
        limit.error_code(),
    )
}

pub(super) fn per_user_run_quota_terminal_events(
    limit: astra_services::resource_governor::ResourceLimitKind,
    reason: &str,
) -> [Value; 2] {
    let error_code = limit.error_code();
    [
        json!({
            "event_type": "run_error",
            "data": {
                "error": reason,
                "error_code": error_code,
                "error_kind": "budget_exhausted",
            },
        }),
        json!({
            "event_type": "run_finished",
            "data": {
                "total_prompt_tokens": 0,
                "total_completion_tokens": 0,
                "error": reason,
                "error_code": error_code,
                "error_kind": "budget_exhausted",
            },
        }),
    ]
}

pub(super) fn classified_terminal_error_code(error: &astra_core::ClassifiedError) -> String {
    if let Some(details_json) = error.details_json.as_deref()
        && let Ok(Value::Object(details)) = serde_json::from_str::<Value>(details_json)
    {
        match details.get("source").and_then(Value::as_str) {
            Some("llm_provider_admission") => {
                return "llm_provider_admission_rejected".to_string();
            }
            Some("work_admission")
                if details.get("error_kind").and_then(Value::as_str)
                    == Some("work_lifecycle_topology_conflict") =>
            {
                return "work_lifecycle_topology_conflict".to_string();
            }
            Some(crate::server::server_loop_host::HOST_EVENT_ROUTER_SOURCE)
                if details.get("error_code").and_then(Value::as_str)
                    == Some(
                        crate::server::server_loop_host::HOST_EVENT_ROUTE_CONTRACT_ERROR_CODE,
                    ) =>
            {
                return crate::server::server_loop_host::HOST_EVENT_ROUTE_CONTRACT_ERROR_CODE
                    .to_string();
            }
            _ => {}
        }
    }
    error.kind.as_str().to_string()
}

pub(super) fn register_run_admission_metrics(
    registry: &astra_turn_core::pipeline_metrics::MetricsRegistry,
) {
    registry.register_counter(
        METRIC_RUN_ADMISSION_ATTEMPTS_TOTAL,
        "Run admission attempts by outcome.",
    );
    registry.register_counter(
        METRIC_RUN_ADMISSION_WAIT_MS_TOTAL,
        "Total milliseconds spent waiting for run admission by outcome.",
    );
    registry.register_counter(
        METRIC_RUN_ADMISSION_WEIGHT_UNITS_TOTAL,
        "Current count-based run-admission units by outcome; Phase 0 records one unit per run regardless of context size.",
    );
}

pub(super) fn register_durable_run_event_metrics(
    registry: &astra_turn_core::pipeline_metrics::MetricsRegistry,
) {
    registry.register_counter(
        METRIC_DURABLE_RUN_EVENT_BATCHES_TOTAL,
        "Durable run event batches by terminal persistence path, outcome, and compaction state.",
    );
    registry.register_counter(
        METRIC_DURABLE_RUN_EVENT_ROWS_TOTAL,
        "Durable run event rows by terminal persistence path, outcome, and compaction state.",
    );
    registry.register_counter(
        METRIC_DURABLE_RUN_EVENT_BYTES_TOTAL,
        "Estimated durable run event bytes by terminal persistence path, outcome, and compaction state.",
    );
    registry.register_gauge(
        METRIC_DURABLE_RUN_EVENT_ROW_BUDGET,
        "Configured maximum durable run event rows per terminal batch.",
    );
    registry.register_gauge(
        METRIC_DURABLE_RUN_EVENT_BYTE_BUDGET,
        "Configured maximum estimated durable run event bytes per terminal batch.",
    );
    refresh_durable_run_event_budget_metrics(registry);
}

pub(super) fn register_post_loop_memory_cleanup_metrics(
    registry: &astra_turn_core::pipeline_metrics::MetricsRegistry,
) {
    registry.register_counter(
        METRIC_POST_LOOP_MEMORY_CLEANUP_DISPATCHES_TOTAL,
        "Post-loop memory cleanup dispatches by mode and low-cardinality outcome.",
    );
    registry.register_counter(
        METRIC_POST_LOOP_MEMORY_CLEANUP_WORKERS_TOTAL,
        "Post-loop memory cleanup workers by low-cardinality outcome.",
    );
    registry.register_counter(
        METRIC_SESSION_MEMORY_POST_LOOP_DRAINS_TOTAL,
        "Post-loop session-memory extraction drains by low-cardinality outcome.",
    );
}

pub(super) fn refresh_durable_run_event_budget_metrics(
    registry: &astra_turn_core::pipeline_metrics::MetricsRegistry,
) {
    let budget = DurableRunEventBatchBudget::default();
    registry.set_gauge(
        METRIC_DURABLE_RUN_EVENT_ROW_BUDGET,
        &[],
        budget.row_budget as f64,
    );
    registry.set_gauge(
        METRIC_DURABLE_RUN_EVENT_BYTE_BUDGET,
        &[],
        budget.byte_budget as f64,
    );
}

pub(super) fn duration_millis_u64(duration: Duration) -> u64 {
    duration.as_millis().try_into().unwrap_or(u64::MAX)
}

pub(super) fn record_durable_run_event_batch_metrics(
    registry: Option<&Arc<astra_turn_core::pipeline_metrics::MetricsRegistry>>,
    path: &'static str,
    outcome: &'static str,
    events: &[Value],
) {
    if events.is_empty() {
        return;
    }
    let Some(registry) = registry else {
        return;
    };
    register_durable_run_event_metrics(registry);
    refresh_durable_run_event_budget_metrics(registry);
    let compacted = events
        .iter()
        .any(|event| durable_event_type(event) == Some("durable_events_compacted"));
    let compacted_label = if compacted { "true" } else { "false" };
    let labels = &[
        ("path", path),
        ("outcome", outcome),
        ("compacted", compacted_label),
    ];
    registry.increment_counter(METRIC_DURABLE_RUN_EVENT_BATCHES_TOTAL, labels, 1);
    registry.increment_counter(
        METRIC_DURABLE_RUN_EVENT_ROWS_TOTAL,
        labels,
        events.len().try_into().unwrap_or(u64::MAX),
    );
    registry.increment_counter(
        METRIC_DURABLE_RUN_EVENT_BYTES_TOTAL,
        labels,
        events
            .iter()
            .map(durable_run_event_estimated_bytes)
            .sum::<usize>()
            .try_into()
            .unwrap_or(u64::MAX),
    );
}

pub(super) fn record_post_loop_memory_cleanup_dispatch_metrics(
    registry: Option<&Arc<astra_turn_core::pipeline_metrics::MetricsRegistry>>,
    mode: &'static str,
    outcome: &'static str,
) {
    let Some(registry) = registry else {
        return;
    };
    register_post_loop_memory_cleanup_metrics(registry);
    registry.increment_counter(
        METRIC_POST_LOOP_MEMORY_CLEANUP_DISPATCHES_TOTAL,
        &[("mode", mode), ("outcome", outcome)],
        1,
    );
}

pub(super) fn record_post_loop_memory_cleanup_worker_metrics(
    registry: Option<&Arc<astra_turn_core::pipeline_metrics::MetricsRegistry>>,
    outcome: &'static str,
) {
    let Some(registry) = registry else {
        return;
    };
    register_post_loop_memory_cleanup_metrics(registry);
    registry.increment_counter(
        METRIC_POST_LOOP_MEMORY_CLEANUP_WORKERS_TOTAL,
        &[("outcome", outcome)],
        1,
    );
}

pub(super) fn record_session_memory_post_loop_drain_metrics(
    registry: Option<&Arc<astra_turn_core::pipeline_metrics::MetricsRegistry>>,
    outcome: &'static str,
) {
    let Some(registry) = registry else {
        return;
    };
    register_post_loop_memory_cleanup_metrics(registry);
    registry.increment_counter(
        METRIC_SESSION_MEMORY_POST_LOOP_DRAINS_TOTAL,
        &[("outcome", outcome)],
        1,
    );
}
