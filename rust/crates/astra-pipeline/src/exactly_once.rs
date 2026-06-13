//! Exactly-once tool execution — prevents duplicate side effects on crash recovery.
//!
//! # Design
//!
//! When a session crashes and recovers, tools may have already executed. Re-executing
//! side-effect tools (bash, github_create_issue, etc.) causes duplicates. This module
//! provides exactly-once semantics:
//!
//! 1. **Before execution**: Check idempotency cache for (tool_name, args) hash
//! 2. **Cache hit**:
//!    - PureRead: optionally re-execute (safe) or return cached
//!    - IdempotentWrite: return cached (overwrite is safe but unnecessary)
//!    - NonIdempotent: **always** return cached (never re-execute blindly)
//! 3. **Cache miss**: Execute tool, record successful result, return
//!
//! # Unhappy Paths
//!
//! - Tool execution panics: surfaced to the caller; failed attempts are not cached
//! - Tool returns error: not cached; failures are not proof that a side effect applied
//! - Repeated failures: temporarily suppressed with a retry lease, never stored as
//!   a successful dedupe result
//! - Concurrent execution: cache is per-session, no cross-session dedup (future: MatrixOne)
//! - Workspace mutation: caller must evict stale PureRead results (see `evict_tool()`)

use crate::step_protocol::{CachedToolResult, IdempotencyKey, InMemoryIdempotencyCache};
use astra_turn_types::{ToolIdempotency, classify_tool_idempotency};
use serde_json::Value;
use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::time::{SystemTime, UNIX_EPOCH};
use thiserror::Error;

const DEFAULT_RETRY_SUPPRESSION_SECS: u64 = 30;
const DEFAULT_MAX_RETRY_TRACKING_ENTRIES: usize = 1024;

fn unix_timestamp_secs() -> u64 {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(duration) => duration.as_secs(),
        Err(error) => {
            tracing::warn!(
                error = %error,
                "system clock predates UNIX_EPOCH; using zero cache timestamp"
            );
            0
        }
    }
}

/// Result of exactly-once execution
#[derive(Debug, Clone, PartialEq)]
pub struct ExecutionResult {
    /// Tool output (stdout, file content, etc.)
    pub output: String,
    /// Whether the tool returned an error
    pub is_error: bool,
    /// Whether this result came from cache (true) or fresh execution (false)
    pub from_cache: bool,
}

/// Errors during exactly-once execution
#[derive(Debug, Error, Clone)]
pub enum ExactlyOnceError {
    #[error("Tool execution panicked: {0}")]
    ToolPanic(String),

    #[error("Cache check failed: {0}")]
    CacheError(String),

    #[error("Tool execution error: {0}")]
    ToolExecutionError(String),

    #[error(
        "Tool retry suppressed after {retry_count} consecutive failures; retry after {retry_after_secs}s: {last_error}"
    )]
    RetrySuppressed {
        retry_count: u32,
        retry_after_secs: u64,
        last_error: String,
    },
}

/// Exactly-once executor wrapping an idempotency cache.
///
/// # Example
///
/// ```rust
/// use astra_pipeline::exactly_once::ExactlyOnceExecutor;
/// use serde_json::json;
///
/// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
/// let mut executor = ExactlyOnceExecutor::new();
///
/// // First call: executes
/// let result1 = executor
///     .execute_with_dedup(
///         "step-1",
///         0,
///         "bash",
///         &json!({"command": "echo hello"}),
///         |tool, args| async move {
///             Ok("hello".to_string())
///         },
///     )
///     .await?;
/// assert!(!result1.from_cache);
///
/// // Second call: returns cached (bash is NonIdempotent)
/// let result2 = executor
///     .execute_with_dedup(
///         "step-1",
///         0,
///         "bash",
///         &json!({"command": "echo hello"}),
///         |tool, args| async move {
///             Ok("hello".to_string())
///         },
///     )
///     .await?;
/// assert!(result2.from_cache);
/// # Ok(())
/// # }
/// ```
pub struct ExactlyOnceExecutor {
    cache: InMemoryIdempotencyCache,
    /// Policy: whether to re-execute PureRead tools on cache hit
    pure_read_policy: PureReadPolicy,
    /// Max consecutive failures for same key before applying retry suppression.
    max_retries: u32,
    /// Consecutive failure counts per key. Reset on first success.
    retry_counts: HashMap<IdempotencyKey, u32>,
    /// Short-lived failure leases used to prevent crash-recovery retry storms
    /// without poisoning the exactly-once result cache.
    retry_suppressions: HashMap<IdempotencyKey, RetrySuppression>,
    /// FIFO index for bounded transient retry state. Stale entries are skipped
    /// during pruning, so clearing a key does not require scanning the queue.
    retry_order: VecDeque<IdempotencyKey>,
    retry_suppression_secs: u64,
    max_retry_tracking_entries: usize,
}

#[derive(Debug, Clone)]
struct RetrySuppression {
    retry_count: u32,
    retry_after_epoch_secs: u64,
    last_error: String,
}

impl RetrySuppression {
    fn remaining_secs(&self, now: u64) -> u64 {
        self.retry_after_epoch_secs.saturating_sub(now)
    }
}

/// Policy for PureRead tools on cache hit
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PureReadPolicy {
    /// Always return cached result (fastest, but may miss workspace changes)
    AlwaysCache,
    /// Re-execute if workspace_version changed (requires ContextSignature)
    ReexecuteOnWorkspaceChange,
}

impl Default for ExactlyOnceExecutor {
    fn default() -> Self {
        Self::new()
    }
}

impl ExactlyOnceExecutor {
    /// Create a new executor with empty cache.
    ///
    /// Default policy: `ReexecuteOnWorkspaceChange` — PureRead tools are re-executed
    /// if workspace_version changed, preventing stale reads after file mutations.
    pub fn new() -> Self {
        Self {
            cache: InMemoryIdempotencyCache::new(),
            pure_read_policy: PureReadPolicy::ReexecuteOnWorkspaceChange,
            max_retries: 5,
            retry_counts: HashMap::new(),
            retry_suppressions: HashMap::new(),
            retry_order: VecDeque::new(),
            retry_suppression_secs: DEFAULT_RETRY_SUPPRESSION_SECS,
            max_retry_tracking_entries: DEFAULT_MAX_RETRY_TRACKING_ENTRIES,
        }
    }

    /// Set max_retries for retry-storm protection. After `max_retries`
    /// consecutive failures for the same tool+args, retries are temporarily
    /// suppressed without caching the failed result.
    pub fn with_max_retries(mut self, max_retries: u32) -> Self {
        self.max_retries = max_retries;
        self
    }

    /// Set the retry suppression lease duration.
    pub fn with_retry_suppression_secs(mut self, retry_suppression_secs: u64) -> Self {
        self.retry_suppression_secs = retry_suppression_secs;
        self
    }

    /// Bound transient retry-state growth for long-running sessions.
    ///
    /// This does not evict successful exactly-once cache entries. A zero value
    /// disables retry-state retention and therefore also disables retry
    /// suppression.
    pub fn with_max_retry_tracking_entries(mut self, max_entries: usize) -> Self {
        self.max_retry_tracking_entries = max_entries;
        self.enforce_retry_tracking_bound();
        self
    }

    /// Set PureRead policy
    pub fn with_pure_read_policy(mut self, policy: PureReadPolicy) -> Self {
        self.pure_read_policy = policy;
        self
    }

    /// Execute a tool with exactly-once semantics.
    ///
    /// # Arguments
    ///
    /// - `step_id`: Current step identifier
    /// - `tool_index`: Index of this tool call within the step
    /// - `tool_name`: Name of the tool (e.g., "bash", "read_file")
    /// - `args`: Tool arguments as JSON
    /// - `executor`: Async function that executes the tool and returns output or error
    ///
    /// # Returns
    ///
    /// `ExecutionResult` indicating whether result came from cache or was freshly executed.
    pub async fn execute_with_dedup<F, Fut>(
        &mut self,
        step_id: &str,
        tool_index: u32,
        tool_name: &str,
        args: &Value,
        executor: F,
    ) -> Result<ExecutionResult, ExactlyOnceError>
    where
        F: FnOnce(String, Value) -> Fut,
        Fut: Future<Output = Result<String, String>>,
    {
        let key = IdempotencyKey::new(step_id, tool_index, tool_name, args);
        let idempotency = classify_tool_idempotency(tool_name, Some(args));

        // Phase 1: Check cache
        if let Some(cached) = self.cache.check(&key) {
            // Cache hit — decide based on tool idempotency
            match idempotency {
                ToolIdempotency::NonIdempotent => {
                    // Side-effect tool: NEVER re-execute
                    tracing::debug!(
                        tool_name = %tool_name,
                        step_id = %step_id,
                        "Exactly-once: cache hit for NonIdempotent tool, returning cached"
                    );
                    return Ok(ExecutionResult {
                        output: cached.output.clone(),
                        is_error: cached.is_error,
                        from_cache: true,
                    });
                }
                ToolIdempotency::IdempotentWrite => {
                    // Overwrite-style write: safe but unnecessary to re-execute
                    tracing::debug!(
                        tool_name = %tool_name,
                        step_id = %step_id,
                        "Exactly-once: cache hit for IdempotentWrite tool, returning cached"
                    );
                    return Ok(ExecutionResult {
                        output: cached.output.clone(),
                        is_error: cached.is_error,
                        from_cache: true,
                    });
                }
                ToolIdempotency::PureRead => {
                    // Pure read: policy decides
                    match self.pure_read_policy {
                        PureReadPolicy::AlwaysCache => {
                            tracing::debug!(
                                tool_name = %tool_name,
                                step_id = %step_id,
                                "Exactly-once: cache hit for PureRead tool (AlwaysCache policy)"
                            );
                            return Ok(ExecutionResult {
                                output: cached.output.clone(),
                                is_error: cached.is_error,
                                from_cache: true,
                            });
                        }
                        PureReadPolicy::ReexecuteOnWorkspaceChange => {
                            // Check if workspace_version changed
                            let should_reexecute = if let Some(current_ctx) = &key.context_signature
                            {
                                if let Some(cached_ctx) = &cached.context_signature {
                                    // Compare workspace versions
                                    current_ctx.workspace_version != cached_ctx.workspace_version
                                } else {
                                    // No cached context, assume workspace may have changed
                                    true
                                }
                            } else {
                                // No current context, use cache (can't verify workspace)
                                false
                            };

                            if should_reexecute {
                                tracing::debug!(
                                    tool_name = %tool_name,
                                    step_id = %step_id,
                                    "Exactly-once: cache hit for PureRead tool, but workspace changed, re-executing"
                                );
                                // Fall through to execute
                            } else {
                                tracing::debug!(
                                    tool_name = %tool_name,
                                    step_id = %step_id,
                                    "Exactly-once: cache hit for PureRead tool (workspace unchanged)"
                                );
                                return Ok(ExecutionResult {
                                    output: cached.output.clone(),
                                    is_error: cached.is_error,
                                    from_cache: true,
                                });
                            }
                        }
                    }
                }
            }
        }

        let now_secs = unix_timestamp_secs();
        if let Some(suppression) = self.retry_suppressions.get(&key) {
            let remaining = suppression.remaining_secs(now_secs);
            if remaining > 0 {
                tracing::warn!(
                    tool_name = %tool_name,
                    step_id = %step_id,
                    retry_count = suppression.retry_count,
                    retry_after_secs = remaining,
                    "Exactly-once: retry suppressed by short-lived failure lease"
                );
                return Err(ExactlyOnceError::RetrySuppressed {
                    retry_count: suppression.retry_count,
                    retry_after_secs: remaining,
                    last_error: suppression.last_error.clone(),
                });
            }
            self.clear_retry_tracking(&key);
        }

        // Phase 2: Cache miss — execute tool
        tracing::debug!(
            tool_name = %tool_name,
            step_id = %step_id,
            "Exactly-once: cache miss, executing tool"
        );

        let result = executor(tool_name.to_string(), args.clone()).await;

        // Phase 3: Record successful result and return. Failed attempts are
        // deliberately not cached: exactly-once protects confirmed side
        // effects, while an error is often retryable transport/runtime state.
        match result {
            Ok(output) => {
                // Success clears the retry counter for this key.
                self.clear_retry_tracking(&key);
                let cached_result = CachedToolResult {
                    tool_name: tool_name.to_string(),
                    output: output.clone(),
                    is_error: false,
                    cached_at: unix_timestamp_secs(),
                    context_signature: key.context_signature.clone(),
                };
                self.cache.record(&key, cached_result);
                Ok(ExecutionResult {
                    output,
                    is_error: false,
                    from_cache: false,
                })
            }
            Err(error_msg) => {
                // Retry-storm protection: after max_retries consecutive
                // failures for the same tool+args, install a short-lived
                // suppression lease. Do not cache the error: a failed attempt
                // is not proof that a side effect completed.
                let Some(count) = self.record_retry_failure(&key) else {
                    return Err(ExactlyOnceError::ToolExecutionError(error_msg));
                };
                if count >= self.max_retries {
                    tracing::warn!(
                        tool_name = %tool_name,
                        step_id = %step_id,
                        retry_count = count,
                        max_retries = self.max_retries,
                        retry_suppression_secs = self.retry_suppression_secs,
                        "Exactly-once: retry-storm guard engaged with short-lived suppression lease"
                    );
                    let retry_after_epoch_secs =
                        unix_timestamp_secs().saturating_add(self.retry_suppression_secs);
                    self.retry_suppressions.insert(
                        key,
                        RetrySuppression {
                            retry_count: count,
                            retry_after_epoch_secs,
                            last_error: error_msg.clone(),
                        },
                    );
                    self.enforce_retry_tracking_bound();
                }
                Err(ExactlyOnceError::ToolExecutionError(error_msg))
            }
        }
    }

    fn clear_retry_tracking(&mut self, key: &IdempotencyKey) {
        self.retry_counts.remove(key);
        self.retry_suppressions.remove(key);
    }

    fn record_retry_failure(&mut self, key: &IdempotencyKey) -> Option<u32> {
        if self.max_retry_tracking_entries == 0 {
            return None;
        }

        if !self.retry_counts.contains_key(key) && !self.retry_suppressions.contains_key(key) {
            self.retry_order.push_back(key.clone());
        }

        let count = self.retry_counts.entry(key.clone()).or_insert(0);
        *count = count.saturating_add(1);
        let count = *count;
        self.enforce_retry_tracking_bound();
        Some(count)
    }

    fn retry_tracking_size(&self) -> usize {
        self.retry_counts.len().max(self.retry_suppressions.len())
    }

    fn enforce_retry_tracking_bound(&mut self) {
        if self.max_retry_tracking_entries == 0 {
            self.retry_counts.clear();
            self.retry_suppressions.clear();
            self.retry_order.clear();
            return;
        }

        while self.retry_tracking_size() > self.max_retry_tracking_entries {
            let Some(key) = self.retry_order.pop_front() else {
                self.retry_counts.clear();
                self.retry_suppressions.clear();
                return;
            };
            self.retry_counts.remove(&key);
            self.retry_suppressions.remove(&key);
        }

        while let Some(key) = self.retry_order.front() {
            if self.retry_counts.contains_key(key) || self.retry_suppressions.contains_key(key) {
                break;
            }
            self.retry_order.pop_front();
        }
    }

    /// Evict all cache entries for a step (after step completes successfully).
    pub fn evict_step(&mut self, step_id: &str) {
        self.cache.evict_step(step_id);
    }

    /// Evict cache entries for specific tools (after workspace mutation).
    pub fn evict_tools(&mut self, tool_names: &[&str]) {
        self.cache.evict_tools(tool_names);
    }

    /// Get reference to underlying cache (for integration with crash recovery).
    pub fn cache(&self) -> &InMemoryIdempotencyCache {
        &self.cache
    }

    /// Get mutable reference to underlying cache (for loading from checkpoint).
    pub fn cache_mut(&mut self) -> &mut InMemoryIdempotencyCache {
        &mut self.cache
    }

    /// Number of cached entries
    pub fn cache_size(&self) -> usize {
        self.cache.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn exactly_once_production_has_no_direct_panic_unwraps() {
        let source = include_str!("exactly_once.rs");
        let production = source.split("#[cfg(test)]").next().unwrap_or(source);
        assert!(
            !production.contains(".expect("),
            "exactly-once production path must not panic on unhappy paths"
        );
        assert!(
            !production.contains(".unwrap("),
            "exactly-once production path must not unwrap fallible runtime state"
        );
    }

    #[tokio::test]
    async fn test_cache_miss_executes_tool() {
        let mut executor = ExactlyOnceExecutor::new();
        let args = json!({"command": "echo hello"});

        let result = executor
            .execute_with_dedup("step-1", 0, "bash", &args, |_, _| async {
                Ok("hello\n".to_string())
            })
            .await
            .unwrap();

        assert!(!result.from_cache);
        assert_eq!(result.output, "hello\n");
        assert!(!result.is_error);
    }

    #[tokio::test]
    async fn test_cache_hit_non_idempotent_returns_cached() {
        let mut executor = ExactlyOnceExecutor::new();
        let args = json!({"command": "echo hello"});

        // First call: executes
        let result1 = executor
            .execute_with_dedup("step-1", 0, "bash", &args, |_, _| async {
                Ok("hello\n".to_string())
            })
            .await
            .unwrap();
        assert!(!result1.from_cache);

        // Second call: returns cached (bash is NonIdempotent)
        let result2 = executor
            .execute_with_dedup("step-1", 0, "bash", &args, |_, _| async {
                Ok("different\n".to_string()) // Should NOT execute this
            })
            .await
            .unwrap();
        assert!(result2.from_cache);
        assert_eq!(result2.output, "hello\n"); // Original result
    }

    #[tokio::test]
    async fn transient_tool_errors_are_not_cached_permanently() {
        let mut executor = ExactlyOnceExecutor::new();
        let args = json!({"command": "curl https://example.invalid"});

        let first = executor
            .execute_with_dedup("step-1", 0, "bash", &args, |_, _| async {
                Err("network timeout while connecting".to_string())
            })
            .await;
        assert!(matches!(
            first,
            Err(ExactlyOnceError::ToolExecutionError(ref error))
                if error.contains("network timeout")
        ));
        assert_eq!(
            executor.cache_size(),
            0,
            "transient execution errors must not become permanent exactly-once cache entries"
        );

        let second = executor
            .execute_with_dedup("step-1", 0, "bash", &args, |_, _| async {
                Ok("retried successfully".to_string())
            })
            .await
            .expect("retry after transient error should execute");
        assert!(!second.from_cache);
        assert_eq!(second.output, "retried successfully");
    }

    #[tokio::test]
    async fn test_cache_hit_idempotent_write_returns_cached() {
        let mut executor = ExactlyOnceExecutor::new();
        let args = json!({"path": "test.txt", "content": "hello"});

        // First call: executes
        let result1 = executor
            .execute_with_dedup("step-1", 0, "write_file", &args, |_, _| async {
                Ok("".to_string())
            })
            .await
            .unwrap();
        assert!(!result1.from_cache);

        // Second call: returns cached (write_file is IdempotentWrite)
        let result2 = executor
            .execute_with_dedup("step-1", 0, "write_file", &args, |_, _| async {
                Ok("different".to_string()) // Should NOT execute this
            })
            .await
            .unwrap();
        assert!(result2.from_cache);
    }

    #[tokio::test]
    async fn test_cache_hit_pure_read_always_cache_policy() {
        let mut executor =
            ExactlyOnceExecutor::new().with_pure_read_policy(PureReadPolicy::AlwaysCache);
        let args = json!({"path": "test.txt"});

        // First call: executes
        let result1 = executor
            .execute_with_dedup("step-1", 0, "read_file", &args, |_, _| async {
                Ok("content v1".to_string())
            })
            .await
            .unwrap();
        assert!(!result1.from_cache);

        // Second call: returns cached (AlwaysCache policy)
        let result2 = executor
            .execute_with_dedup("step-1", 0, "read_file", &args, |_, _| async {
                Ok("content v2".to_string()) // Should NOT execute this
            })
            .await
            .unwrap();
        assert!(result2.from_cache);
        assert_eq!(result2.output, "content v1");
    }

    #[tokio::test]
    async fn test_error_is_not_cached_below_retry_limit() {
        let mut executor = ExactlyOnceExecutor::new().with_max_retries(3);
        let args = json!({"command": "exit 1"});

        // Fail 2 times — still below max_retries, errors not cached
        for i in 0..2 {
            let result = executor
                .execute_with_dedup("step-1", 0, "bash", &args, |_, _| async {
                    Err(format!("command failed attempt {}", i + 1))
                })
                .await;
            assert!(result.is_err(), "attempt {} should fail", i + 1);
        }
        assert_eq!(
            executor.cache_size(),
            0,
            "errors not cached below max_retries"
        );

        // Retry succeeds — cache empty because previous were errors
        let result = executor
            .execute_with_dedup("step-1", 0, "bash", &args, |_, _| async {
                Ok("success".to_string())
            })
            .await
            .unwrap();
        assert!(!result.from_cache);
        assert!(!result.is_error);
        assert_eq!(result.output, "success");
    }

    #[tokio::test]
    async fn test_retry_storm_guard_engages_without_poisoning_cache() {
        let mut executor = ExactlyOnceExecutor::new()
            .with_max_retries(3)
            .with_retry_suppression_secs(60);
        let args = json!({"command": "curl https://down.example"});

        // Fail 3 times — hits max_retries, but the error is not cached as an
        // exactly-once result.
        for _i in 0..3 {
            let result = executor
                .execute_with_dedup("step-1", 0, "bash", &args, |_, _| async {
                    Err("network timeout".to_string())
                })
                .await;
            assert!(result.is_err());
        }
        assert_eq!(
            executor.cache_size(),
            0,
            "retry suppression must not poison the exactly-once result cache"
        );

        // 4th attempt: no tool execution while the suppression lease is fresh.
        let mut executed = false;
        let suppressed = executor
            .execute_with_dedup("step-1", 0, "bash", &args, |_, _| async {
                executed = true;
                Ok("should not execute".to_string())
            })
            .await;
        assert!(
            matches!(suppressed, Err(ExactlyOnceError::RetrySuppressed { .. })),
            "4th attempt should be suppressed by a failure lease"
        );
        assert!(!executed, "suppressed retry must not call the tool");
        assert_eq!(executor.cache_size(), 0);
    }

    #[tokio::test]
    async fn retry_suppression_expiry_allows_successful_recovery() {
        let mut executor = ExactlyOnceExecutor::new()
            .with_max_retries(2)
            .with_retry_suppression_secs(0);
        let args = json!({"command": "curl https://temporarily-down.example"});

        for _ in 0..2 {
            let result = executor
                .execute_with_dedup("step-1", 0, "bash", &args, |_, _| async {
                    Err("network timeout".to_string())
                })
                .await;
            assert!(result.is_err());
        }

        assert_eq!(executor.cache_size(), 0);
        let recovered = executor
            .execute_with_dedup("step-1", 0, "bash", &args, |_, _| async {
                Ok("network recovered".to_string())
            })
            .await
            .expect("expired suppression should execute the retry");
        assert!(!recovered.from_cache);
        assert_eq!(recovered.output, "network recovered");
        assert_eq!(executor.cache_size(), 1);
    }

    #[tokio::test]
    async fn transient_failure_tracking_is_bounded_for_long_sessions() {
        let mut executor = ExactlyOnceExecutor::new();

        for index in 0..1100 {
            let args = json!({"command": format!("curl https://down-{index}.example")});
            let result = executor
                .execute_with_dedup("step-long-session", index, "bash", &args, |_, _| async {
                    Err("temporary network timeout".to_string())
                })
                .await;
            assert!(matches!(
                result,
                Err(ExactlyOnceError::ToolExecutionError(_))
            ));
        }

        assert_eq!(
            executor.cache_size(),
            0,
            "transient failures must not enter the durable exactly-once cache"
        );
        assert!(
            executor.retry_counts.len() <= DEFAULT_MAX_RETRY_TRACKING_ENTRIES,
            "long sessions need a bounded transient retry-tracking table"
        );
    }

    #[tokio::test]
    async fn retry_suppression_tracking_respects_configured_bound() {
        let mut executor = ExactlyOnceExecutor::new()
            .with_max_retries(1)
            .with_max_retry_tracking_entries(3)
            .with_retry_suppression_secs(60);

        for index in 0..10 {
            let args = json!({"command": format!("curl https://down-{index}.example")});
            let result = executor
                .execute_with_dedup("step-suppressed", index, "bash", &args, |_, _| async {
                    Err("temporary network timeout".to_string())
                })
                .await;
            assert!(matches!(
                result,
                Err(ExactlyOnceError::ToolExecutionError(_))
            ));
        }

        assert!(
            executor.retry_counts.len() <= 3,
            "failure counters should obey the configured transient-state bound"
        );
        assert!(
            executor.retry_suppressions.len() <= 3,
            "retry suppression leases should obey the same transient-state bound"
        );
        assert_eq!(
            executor.cache_size(),
            0,
            "suppression leases are runtime state, not durable exactly-once results"
        );
    }

    #[tokio::test]
    async fn successful_retry_clears_transient_failure_tracking() {
        let mut executor = ExactlyOnceExecutor::new().with_max_retries(2);
        let args = json!({"command": "curl https://flaky.example"});
        let key = IdempotencyKey::new("step-flaky", 0, "bash", &args);

        let failed = executor
            .execute_with_dedup("step-flaky", 0, "bash", &args, |_, _| async {
                Err("temporary network timeout".to_string())
            })
            .await;
        assert!(matches!(
            failed,
            Err(ExactlyOnceError::ToolExecutionError(_))
        ));
        assert!(executor.retry_counts.contains_key(&key));

        let recovered = executor
            .execute_with_dedup("step-flaky", 0, "bash", &args, |_, _| async {
                Ok("recovered".to_string())
            })
            .await
            .expect("retry after transient failure should execute");

        assert_eq!(recovered.output, "recovered");
        assert!(!executor.retry_counts.contains_key(&key));
        assert!(!executor.retry_suppressions.contains_key(&key));
    }

    #[tokio::test]
    async fn test_different_args_different_cache_entries() {
        let mut executor = ExactlyOnceExecutor::new();

        // First call with args A
        let result1 = executor
            .execute_with_dedup(
                "step-1",
                0,
                "bash",
                &json!({"command": "echo A"}),
                |_, _| async { Ok("A\n".to_string()) },
            )
            .await
            .unwrap();
        assert!(!result1.from_cache);

        // Second call with args B (different args → cache miss)
        let result2 = executor
            .execute_with_dedup(
                "step-1",
                1,
                "bash",
                &json!({"command": "echo B"}),
                |_, _| async { Ok("B\n".to_string()) },
            )
            .await
            .unwrap();
        assert!(!result2.from_cache);
        assert_eq!(result2.output, "B\n");
    }

    #[tokio::test]
    async fn test_evict_step_clears_cache() {
        let mut executor = ExactlyOnceExecutor::new();
        let args = json!({"command": "echo hello"});

        // Execute and cache
        executor
            .execute_with_dedup("step-1", 0, "bash", &args, |_, _| async {
                Ok("hello\n".to_string())
            })
            .await
            .unwrap();

        assert_eq!(executor.cache_size(), 1);

        // Evict step
        executor.evict_step("step-1");

        assert_eq!(executor.cache_size(), 0);

        // Next call executes again
        let result = executor
            .execute_with_dedup("step-1", 0, "bash", &args, |_, _| async {
                Ok("hello again\n".to_string())
            })
            .await
            .unwrap();
        assert!(!result.from_cache);
    }

    #[tokio::test]
    async fn test_evict_tools_clears_specific_tools() {
        let mut executor = ExactlyOnceExecutor::new();

        // Cache bash
        executor
            .execute_with_dedup(
                "step-1",
                0,
                "bash",
                &json!({"command": "echo A"}),
                |_, _| async { Ok("A\n".to_string()) },
            )
            .await
            .unwrap();

        // Cache read_file
        executor
            .execute_with_dedup(
                "step-1",
                1,
                "read_file",
                &json!({"path": "test.txt"}),
                |_, _| async { Ok("content".to_string()) },
            )
            .await
            .unwrap();

        assert_eq!(executor.cache_size(), 2);

        // Evict only bash
        executor.evict_tools(&["bash"]);

        assert_eq!(executor.cache_size(), 1);
    }

    #[tokio::test]
    async fn test_concurrent_execution_same_key() {
        // This test verifies that concurrent calls with the same key don't cause issues.
        // In practice, ExactlyOnceExecutor is per-session, so concurrent calls within
        // a session are serialized by the async runtime. Cross-session dedup is future work.
        let mut executor = ExactlyOnceExecutor::new();
        let args = json!({"command": "echo hello"});

        // First call
        let result1 = executor
            .execute_with_dedup("step-1", 0, "bash", &args, |_, _| async {
                Ok("hello\n".to_string())
            })
            .await
            .unwrap();
        assert!(!result1.from_cache);

        // Second call (sequential, not truly concurrent, but tests the cache)
        let result2 = executor
            .execute_with_dedup("step-1", 0, "bash", &args, |_, _| async {
                Ok("different\n".to_string())
            })
            .await
            .unwrap();
        assert!(result2.from_cache);
    }

    #[tokio::test]
    async fn test_tool_classification_integration() {
        let mut executor = ExactlyOnceExecutor::new();

        // Test NonIdempotent (bash)
        let bash_result = executor
            .execute_with_dedup(
                "step-1",
                0,
                "bash",
                &json!({"command": "ls"}),
                |_, _| async { Ok("file1\nfile2\n".to_string()) },
            )
            .await
            .unwrap();
        assert!(!bash_result.from_cache);

        let bash_cached = executor
            .execute_with_dedup(
                "step-1",
                0,
                "bash",
                &json!({"command": "ls"}),
                |_, _| async { Ok("different".to_string()) },
            )
            .await
            .unwrap();
        assert!(bash_cached.from_cache);

        // Test IdempotentWrite (write_file)
        let write_result = executor
            .execute_with_dedup(
                "step-1",
                1,
                "write_file",
                &json!({"path": "test.txt", "content": "hello"}),
                |_, _| async { Ok("".to_string()) },
            )
            .await
            .unwrap();
        assert!(!write_result.from_cache);

        let write_cached = executor
            .execute_with_dedup(
                "step-1",
                1,
                "write_file",
                &json!({"path": "test.txt", "content": "hello"}),
                |_, _| async { Ok("different".to_string()) },
            )
            .await
            .unwrap();
        assert!(write_cached.from_cache);

        // Test PureRead (read_file)
        let read_result = executor
            .execute_with_dedup(
                "step-1",
                2,
                "read_file",
                &json!({"path": "test.txt"}),
                |_, _| async { Ok("content".to_string()) },
            )
            .await
            .unwrap();
        assert!(!read_result.from_cache);

        let read_cached = executor
            .execute_with_dedup(
                "step-1",
                2,
                "read_file",
                &json!({"path": "test.txt"}),
                |_, _| async { Ok("different".to_string()) },
            )
            .await
            .unwrap();
        assert!(read_cached.from_cache); // AlwaysCache policy
    }
}
