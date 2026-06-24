//! Capability provider trait and request/response types.
//!
//! The `CapabilityProvider` trait is the core abstraction for tool execution
//! backends.  Every provider (server-builtin, edge connection, sandbox runner,
//! MCP server) implements this trait.

use astra_runtime_env::IsolationIntent;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use super::types::{ProviderKind, ToolCapability};
use crate::storage::StorageAccess;

// ---------------------------------------------------------------------------
// ToolRequest — what the orchestrator sends to a provider
// ---------------------------------------------------------------------------

/// A request to execute a tool through a capability provider.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolRequest {
    /// Which tool capability is being invoked.
    pub capability: ToolCapability,
    /// The concrete tool name (e.g. "memory", "bash", "web_search").
    /// For `Named` capabilities this mirrors the capability; for `Category`
    /// capabilities the caller must supply the specific tool.
    pub tool_name: String,
    /// Unique tool-call identifier (for correlation in logs / traces).
    pub tool_call_id: String,
    /// Tool parameters as a JSON value.
    pub parameters: serde_json::Value,
    /// Minimum isolation level required by the orchestrator.
    pub isolation_required: IsolationIntent,
    /// Optional storage requirement — when set, the provider must
    /// have access to the given path / volume.
    pub storage: Option<StorageAccess>,
    /// Session-scoped user identity — required for edge transport routing
    /// and audit trails across provider boundaries.
    pub user_id: String,
    /// Session-scoped run identifier.
    pub run_id: String,
    /// Session-scoped session identifier.
    pub session_id: String,
}

// ---------------------------------------------------------------------------
// ToolResult — what the provider returns
// ---------------------------------------------------------------------------

/// Result of a tool execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ToolResult {
    /// Successful execution.
    Success {
        /// Structured output data (tool-specific).
        data: serde_json::Value,
        /// Captured stdout.
        stdout: String,
        /// Captured stderr.
        stderr: String,
        /// Exit code (0 = success).
        exit_code: i32,
        /// Structured tool metadata propagated from the execution backend.
        metadata: Option<serde_json::Map<String, serde_json::Value>>,
    },
    /// Execution failed.
    Error {
        /// Human-readable error message.
        message: String,
        /// Whether the caller may retry with the same parameters.
        retryable: bool,
        /// Process exit code when the backend exposed one.
        exit_code: Option<i32>,
        /// Structured tool metadata propagated from the execution backend.
        metadata: Option<serde_json::Map<String, serde_json::Value>>,
    },
}

// ---------------------------------------------------------------------------
// ProviderError — errors from provider lifecycle operations
// ---------------------------------------------------------------------------

/// Errors that can occur during provider lifecycle (health check, etc.).
#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
    /// Provider cannot handle the requested capability.
    #[error("provider is not capable of {capability:?}")]
    NotCapable { capability: ToolCapability },

    /// Execution timed out.
    #[error("tool execution timed out")]
    Timeout,

    /// Provider is unhealthy.
    #[error("provider is unhealthy: {0}")]
    Unhealthy(String),

    /// Storage access problem.
    #[error("storage error: {0}")]
    Storage(String),

    /// Isolation constraint violation.
    #[error("isolation error: {0}")]
    Isolation(String),

    /// Internal provider failure.
    #[error("internal provider error: {0}")]
    Internal(String),
}

// ---------------------------------------------------------------------------
// ServerToolRuntime — server-local tool execution delegate
// ---------------------------------------------------------------------------

/// Runtime that can execute server-local tools (memory, task, session,
/// symbols, web_search, web_fetch, github, introspect, agent, mcp).
///
/// ServerToolExecutor implements this so ServerBuiltinProvider can
/// delegate builtin tool calls without knowing the full executor structure.
#[async_trait]
pub trait ServerToolRuntime: Send + Sync {
    /// Execute a tool call on the server (in-process or RPC).
    async fn execute_local_tool(&self, name: &str, args: &serde_json::Value) -> ToolResult;
}

// ---------------------------------------------------------------------------
// CapabilityProvider — the trait every executor backend implements
// ---------------------------------------------------------------------------

/// A capability provider is any backend that can execute tool calls.
///
/// Implementors include:
/// - Server-builtin tool handlers
/// - Edge-connection remote executors (CLI process)
/// - Sandbox runners (Firecracker, Docker)
/// - MCP server bridges
#[async_trait]
pub trait CapabilityProvider: Send + Sync {
    /// The kind of provider (server, edge, sandbox, MCP).
    fn kind(&self) -> ProviderKind;

    /// Capabilities this provider can fulfill.
    async fn capabilities(&self) -> Vec<ToolCapability>;

    /// Check whether the provider is healthy.
    async fn health_check(&self) -> Result<(), ProviderError>;

    /// Execute a tool request.
    ///
    /// `cancel_token` is an optional cooperative cancellation handle. Providers
    /// SHOULD observe it: check `is_cancelled()` before dispatching and/or race
    /// the underlying I/O against `token.cancelled()` in a `tokio::select!`.
    /// Passing `None` means the caller has no cancellation surface (e.g. tests).
    async fn execute(
        &self,
        request: ToolRequest,
        cancel_token: Option<&std::sync::Arc<tokio_util::sync::CancellationToken>>,
    ) -> ToolResult;

    /// Routing priority (lower = preferred).
    fn priority(&self) -> u8;

    /// Isolation level this provider can offer.
    fn isolation_level(&self) -> IsolationIntent;

    /// Whether this provider can access workspace storage.
    /// Relevant for storage-aware routing.
    async fn storage_accessible(&self) -> bool;
}
