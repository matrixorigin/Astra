//! Shared tool admission, argument parsing, and concurrency classification.
//! Scheduling belongs to the production tool-batch executor.

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use serde_json::Value;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

const METRIC_TOOL_EXECUTION_ATTEMPTS_TOTAL: &str = "astra_tool_execution_attempts_total";
const METRIC_TOOL_EXECUTION_WAIT_MS_TOTAL: &str = "astra_tool_execution_wait_ms_total";

/// Maximum number of read-only tools that can execute concurrently.
/// Override with `ASTRA_MAX_CONCURRENT_TOOL_EXECUTIONS` env var.
pub const DEFAULT_MAX_CONCURRENT_TOOL_EXECUTIONS: usize = 10;

/// Effective max concurrent tool executions — reads `ASTRA_MAX_CONCURRENT_TOOL_EXECUTIONS`
/// env var at process start, falling back to [`DEFAULT_MAX_CONCURRENT_TOOL_EXECUTIONS`].
pub fn max_concurrent_tool_executions() -> usize {
    static CELL: OnceLock<usize> = OnceLock::new();
    *CELL.get_or_init(|| {
        std::env::var("ASTRA_MAX_CONCURRENT_TOOL_EXECUTIONS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(DEFAULT_MAX_CONCURRENT_TOOL_EXECUTIONS)
    })
}

/// Process-wide capacity shared across tool batches, turns, and sessions.
pub fn shared_tool_semaphore() -> Arc<Semaphore> {
    static CELL: OnceLock<Arc<Semaphore>> = OnceLock::new();
    CELL.get_or_init(|| Arc::new(Semaphore::new(max_concurrent_tool_executions())))
        .clone()
}

/// Failure to obtain a slot from the process-wide tool execution budget.
#[derive(Debug, thiserror::Error)]
pub enum ToolPermitError {
    /// The shared semaphore was closed, so no further tool admissions are possible.
    #[error("global tool execution admission is closed")]
    AdmissionClosed,
}

/// Acquire one slot from the process-wide tool execution budget while
/// preserving the same wait/closed metrics across all runtime frontends.
pub async fn acquire_shared_tool_permit() -> Result<OwnedSemaphorePermit, ToolPermitError> {
    acquire_tool_permit(shared_tool_semaphore()).await
}

pub fn set_tool_execution_metrics_registry(
    registry: Arc<crate::pipeline_metrics::MetricsRegistry>,
) {
    register_tool_execution_metrics(&registry);
    let slot = TOOL_EXECUTION_METRICS_REGISTRY.get_or_init(Default::default);
    *slot
        .write()
        .expect("tool execution metrics registry lock poisoned") = Some(registry);
}

static TOOL_EXECUTION_METRICS_REGISTRY: OnceLock<
    std::sync::RwLock<Option<Arc<crate::pipeline_metrics::MetricsRegistry>>>,
> = OnceLock::new();

fn register_tool_execution_metrics(registry: &crate::pipeline_metrics::MetricsRegistry) {
    registry.register_counter(
        METRIC_TOOL_EXECUTION_ATTEMPTS_TOTAL,
        "Tool execution semaphore admission attempts by outcome.",
    );
    registry.register_counter(
        METRIC_TOOL_EXECUTION_WAIT_MS_TOTAL,
        "Total milliseconds spent waiting for tool execution semaphore admission by outcome.",
    );
}

fn tool_metrics_registry() -> Option<Arc<crate::pipeline_metrics::MetricsRegistry>> {
    TOOL_EXECUTION_METRICS_REGISTRY
        .get_or_init(Default::default)
        .read()
        .expect("tool execution metrics registry lock poisoned")
        .clone()
}

fn duration_millis_u64(duration: Duration) -> u64 {
    duration.as_millis().try_into().unwrap_or(u64::MAX)
}

fn record_tool_admission(outcome: &'static str, wait: Duration) {
    let Some(registry) = tool_metrics_registry() else {
        return;
    };
    register_tool_execution_metrics(&registry);
    registry.increment_counter(
        METRIC_TOOL_EXECUTION_ATTEMPTS_TOTAL,
        &[("outcome", outcome)],
        1,
    );
    registry.increment_counter(
        METRIC_TOOL_EXECUTION_WAIT_MS_TOTAL,
        &[("outcome", outcome)],
        duration_millis_u64(wait),
    );
}

async fn acquire_tool_permit(
    semaphore: Arc<Semaphore>,
) -> Result<OwnedSemaphorePermit, ToolPermitError> {
    let start = Instant::now();
    match semaphore.acquire_owned().await {
        Ok(permit) => {
            record_tool_admission("acquired", start.elapsed());
            Ok(permit)
        }
        Err(_closed) => {
            record_tool_admission("closed", start.elapsed());
            Err(ToolPermitError::AdmissionClosed)
        }
    }
}

// ───────────────────────────── Tool Classification ──────────────────────

/// Parse the `arguments` field from a tool call into a `serde_json::Value`.
/// Returns `None` if the field is missing or not parseable.
pub fn parse_tool_args(tc: &Value) -> Option<Value> {
    let raw = tc.get("function").and_then(|f| f.get("arguments"))?;

    match raw {
        Value::Object(_) => Some(raw.clone()),
        Value::String(s) => serde_json::from_str(s).ok(),
        _ => None,
    }
}

/// Classify a tool as parallelizable by name using the canonical classifier.
/// Unknown tools are not parallelizable.
pub fn is_read_only_tool(tool_name: &str) -> bool {
    crate::tool::categories::classify_name(tool_name).parallelizable
}

/// Args-aware variant: classify a tool call as parallelizable, inspecting
/// the command argument for shell tools.
pub fn is_read_only_tool_with_args(tool_name: &str, args: Option<&Value>) -> bool {
    crate::tool::categories::classify(tool_name, args).parallelizable
}

/// Result of a single tool execution.
#[derive(Debug, Clone)]
pub struct ToolExecResult {
    /// Original index in the tool_calls array.
    pub original_index: usize,
    /// Tool call ID (for matching with LLM response).
    pub call_id: String,
    /// Tool name.
    pub tool_name: String,
    /// The result content (success or error).
    pub content: String,
    /// Whether the tool execution succeeded.
    pub success: bool,
}

/// Type alias for the async tool executor function.
/// Takes a tool call Value and returns (call_id, tool_name, content, success).
pub type ToolExecutorFn = Arc<
    dyn Fn(Value) -> Pin<Box<dyn Future<Output = (String, String, String, bool)> + Send>>
        + Send
        + Sync,
>;

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    async fn metrics_registry_test_guard() -> tokio::sync::MutexGuard<'static, ()> {
        static LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
        LOCK.lock().await
    }

    fn metric_value(rendered: &str, prefix: &str) -> f64 {
        rendered
            .lines()
            .find_map(|line| {
                line.strip_prefix(prefix)
                    .and_then(|value| value.trim().parse::<f64>().ok())
            })
            .unwrap_or_else(|| {
                panic!("missing metric line starting with `{prefix}` in:\n{rendered}")
            })
    }

    #[tokio::test(flavor = "current_thread")]
    async fn tool_admission_metrics_record_wait_time() {
        let _guard = metrics_registry_test_guard().await;
        let registry = Arc::new(crate::pipeline_metrics::MetricsRegistry::new());
        set_tool_execution_metrics_registry(registry.clone());
        let semaphore = Arc::new(Semaphore::new(1));
        let held = semaphore.acquire().await.expect("initial permit");
        let mut waiter = Box::pin(acquire_tool_permit(semaphore.clone()));
        std::future::poll_fn(|cx| {
            assert!(waiter.as_mut().poll(cx).is_pending());
            std::task::Poll::Ready(())
        })
        .await;
        tokio::time::sleep(std::time::Duration::from_millis(15)).await;
        drop(held);
        let _permit = waiter.await.expect("waiter should acquire after release");

        let rendered = registry.render_prometheus();
        let attempts = metric_value(
            &rendered,
            "astra_tool_execution_attempts_total{outcome=\"acquired\"}",
        );
        let wait_ms = metric_value(
            &rendered,
            "astra_tool_execution_wait_ms_total{outcome=\"acquired\"}",
        );
        assert!(attempts >= 1.0, "{rendered}");
        assert!(wait_ms > 0.0, "{rendered}");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn tool_admission_closed_semaphore_is_counted() {
        let _guard = metrics_registry_test_guard().await;
        let registry = Arc::new(crate::pipeline_metrics::MetricsRegistry::new());
        set_tool_execution_metrics_registry(registry.clone());
        let semaphore = Arc::new(Semaphore::new(1));
        semaphore.close();

        let result = acquire_tool_permit(semaphore).await;

        assert!(result.is_err());
        let rendered = registry.render_prometheus();
        let closed = metric_value(
            &rendered,
            "astra_tool_execution_attempts_total{outcome=\"closed\"}",
        );
        assert!(closed >= 1.0, "{rendered}");
    }

    #[test]
    fn parse_tool_args_string_arguments() {
        let tc = json!({
            "function": {
                "name": "bash",
                "arguments": "{\"command\": \"git status\"}"
            }
        });
        let parsed = parse_tool_args(&tc).unwrap();
        assert_eq!(parsed["command"], "git status");
    }

    #[test]
    fn parse_tool_args_object_arguments() {
        let tc = json!({
            "function": {
                "name": "bash",
                "arguments": {"command": "ls -la"}
            }
        });
        let parsed = parse_tool_args(&tc).unwrap();
        assert_eq!(parsed["command"], "ls -la");
    }

    #[test]
    fn parse_tool_args_missing_returns_none() {
        let tc = json!({"function": {"name": "bash"}});
        assert!(parse_tool_args(&tc).is_none());
    }
}
