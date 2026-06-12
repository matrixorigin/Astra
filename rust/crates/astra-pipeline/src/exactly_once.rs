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
//! - Concurrent execution: cache is per-session, no cross-session dedup (future: MatrixOne)
//! - Workspace mutation: caller must evict stale PureRead results (see `evict_tool()`)

use crate::step_protocol::{CachedToolResult, IdempotencyKey, InMemoryIdempotencyCache};
use astra_turn_types::{ToolIdempotency, classify_tool_idempotency};
use serde_json::Value;
use std::future::Future;
use std::time::{SystemTime, UNIX_EPOCH};
use thiserror::Error;

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
        }
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
            Err(error_msg) => Err(ExactlyOnceError::ToolExecutionError(error_msg)),
        }
    }

    /// Record an error result (for tools that failed during original execution).
    ///
    /// Errors are intentionally not written to the exactly-once cache. A
    /// failure is not proof that a side effect was applied, and caching it
    /// would convert transient network/runtime failures into permanent replay
    /// failures. The method remains as a semantic no-op for callers that
    /// report failed original executions.
    pub fn record_error(
        &mut self,
        _step_id: &str,
        _tool_index: u32,
        _tool_name: &str,
        _args: &Value,
        _error: String,
    ) {
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

    #[test]
    fn record_error_skips_transient_failures() {
        let mut executor = ExactlyOnceExecutor::new();
        let args = json!({"command": "curl https://example.invalid"});
        executor.record_error(
            "step-1",
            0,
            "bash",
            &args,
            "transport disconnected: timed out".to_string(),
        );

        assert_eq!(
            executor.cache_size(),
            0,
            "record_error must not freeze retryable transport failures"
        );
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
    async fn test_error_is_not_cached() {
        let mut executor = ExactlyOnceExecutor::new();
        let args = json!({"command": "exit 1"});

        // First call: returns error
        let result1 = executor
            .execute_with_dedup("step-1", 0, "bash", &args, |_, _| async {
                Err("command failed".to_string())
            })
            .await;

        // Error should be propagated
        assert!(result1.is_err());

        // Manually recording the error must still leave the cache empty.
        executor.record_error("step-1", 0, "bash", &args, "command failed".to_string());
        assert_eq!(executor.cache_size(), 0);

        // Second call: executes again, because failed attempts are retryable.
        let result2 = executor
            .execute_with_dedup("step-1", 0, "bash", &args, |_, _| async {
                Ok("success".to_string())
            })
            .await
            .unwrap();
        assert!(!result2.from_cache);
        assert!(!result2.is_error);
        assert_eq!(result2.output, "success");
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
