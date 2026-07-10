//! Unified error classification for the astra-engine runtime.
//!
//! Every internal error should be classified exactly once at its source into an
//! [`ErrorKind`]. Downstream code should pattern-match on the kind instead of
//! re-parsing display strings.
//!
//! Boundary outputs that are inherently unstructured still need explicit
//! fallback classifiers:
//! - [`classify_llm_error_message`] for provider / transport strings when the
//!   caller did not receive structured metadata.
//! - [`classify_tool_output`] for external tool output (bash, MCP).
//! - [`classify_model_resolution_error_message`] for model registry lookup
//!   failures returned as strings by legacy service boundaries.

use serde::{Deserialize, Serialize};

/// Unified error classification.
///
/// Covers LLM provider errors, streaming failures, budget/limit exhaustion,
/// network issues, tool errors, and client-side cancellation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorKind {
    // ── LLM provider ─────────────────────────────────
    /// 429, TPM/RPM exceeded.
    RateLimit,
    /// 5xx from provider.
    ServerError,
    /// 401/403, bad API key or expired token.
    Auth,
    /// Prompt too long for the model's context window.
    ContextWindow,
    /// 400 other (duplicate function name, bad schema, malformed request).
    InvalidRequest,

    // ── Streaming ────────────────────────────────────
    /// No SSE chunk received within the idle timeout.
    StreamIdle,
    /// Connection reset, TLS failure, or other transport error mid-stream.
    StreamTransport,

    // ── Connection pool ──────────────────────────────
    /// HTTP connection pool exhausted (reqwest/hyper). The client's
    /// connection pool is saturated — new requests wait for a free
    /// connection until the pool timeout fires. Distinct from
    /// [`DatabaseError`] which covers SQLx / MatrixOne pool failures.
    ConnectionPoolExhausted,

    // ── Budget / limits ──────────────────────────────
    /// Total LLM time budget for the turn/session exhausted.
    BudgetExhausted,
    /// Maximum tool rounds per turn exceeded.
    ToolRoundsExhausted,

    // ── Network ──────────────────────────────────────
    /// DNS failure, connection refused, host unreachable.
    Network,

    // ── Tool errors ──────────────────────────────────
    /// Tool or resource not found (unknown tool, file 404).
    ToolNotFound,
    /// Bad arguments, parse error, type mismatch, workspace read-before-write.
    ToolInvalidArgs,
    /// Local command timed out (grep on huge repo, long-running bash).
    ToolTimeout,
    /// Tool not installed, not configured, or explicitly unavailable.
    ToolUnavailable,
    /// Runtime advertised a tool but failed to bind an executor/transport for it.
    ToolBinding,
    /// OOM, disk full, fork exhaustion, too many open files.
    ResourceLimit,

    // ── Domain ───────────────────────────────────────
    /// MatrixOne / SQLx failure: SQL syntax, deadlock, pool exhausted,
    /// connection lost mid-query. Distinct from `Network` because the fix
    /// lives in the query / schema / pool layer, not network infra.
    DatabaseError,
    /// Agent-side stall: repeated identical tool calls, no progress gradient,
    /// infinite replan. Not a transport or tool error — the remedy is rewind
    /// or model switch.
    Stall,
    /// No concrete model was selected by the caller. The runtime must not
    /// choose an arbitrary active model as a fallback.
    MissingModelSelection,

    // ── Client-side ──────────────────────────────────
    /// User Ctrl-C, cancel token, or API cancellation.
    Cancelled,

    // ── Catch-all ────────────────────────────────────
    /// Unrecognized error.
    Unknown,
}

/// Source-authored cause of a tool failure. This is evidence, not an execution
/// command: downstream runtimes may render it for the model but must not turn
/// advisory recovery actions into hidden retries, tool suppression, or aborts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolFailureCause {
    InvalidArguments,
    InputTooLarge,
    ScopeTooBroad,
    ResourceMissing,
    PermissionBoundary,
    CapabilityUnavailable,
    TransientTransport,
    ResourceExhausted,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolRecoveryAction {
    CorrectArguments,
    ReadTargetedRange,
    NarrowScope,
    SearchBeforeRead,
    VerifyResource,
    RefreshCredentials,
    SelectAvailableCapability,
    WaitAndRetry,
    ReduceResourcePressure,
    InspectStructuredFailure,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolFailureEvidence {
    pub kind: ErrorKind,
    pub cause: ToolFailureCause,
    pub retryable: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub recovery_actions: Vec<ToolRecoveryAction>,
}

impl ToolFailureEvidence {
    pub fn new(
        kind: ErrorKind,
        cause: ToolFailureCause,
        retryable: bool,
        recovery_actions: Vec<ToolRecoveryAction>,
    ) -> Self {
        Self {
            kind,
            cause,
            retryable,
            recovery_actions,
        }
    }
}

impl ErrorKind {
    /// Stable string tag for journal/serialization.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::RateLimit => "rate_limit",
            Self::ServerError => "server_error",
            Self::Auth => "auth",
            Self::ContextWindow => "context_window",
            Self::InvalidRequest => "invalid_request",
            Self::StreamIdle => "stream_idle",
            Self::StreamTransport => "stream_transport",
            Self::ConnectionPoolExhausted => "connection_pool_exhausted",
            Self::BudgetExhausted => "budget_exhausted",
            Self::ToolRoundsExhausted => "tool_rounds_exhausted",
            Self::Network => "network",
            Self::ToolNotFound => "tool_not_found",
            Self::ToolInvalidArgs => "tool_invalid_args",
            Self::ToolTimeout => "tool_timeout",
            Self::ToolUnavailable => "tool_unavailable",
            Self::ToolBinding => "tool_binding",
            Self::ResourceLimit => "resource_limit",
            Self::DatabaseError => "database_error",
            Self::Stall => "stall",
            Self::MissingModelSelection => "missing_model_selection",
            Self::Cancelled => "cancelled",
            Self::Unknown => "unknown",
        }
    }

    /// Whether the error is worth retrying automatically.
    #[must_use]
    pub fn is_retryable(self) -> bool {
        matches!(
            self,
            Self::RateLimit
                | Self::ServerError
                | Self::StreamIdle
                | Self::StreamTransport
                | Self::ConnectionPoolExhausted
                | Self::Network
        )
    }

    /// Suggested delay in milliseconds before retry attempt `attempt` (0-based).
    /// Returns `None` for non-retryable errors.
    #[must_use]
    pub fn retry_delay_ms(self, attempt: usize) -> Option<u64> {
        if !self.is_retryable() {
            return None;
        }
        let base = match self {
            Self::RateLimit => 5_000,
            Self::ServerError => 2_000,
            Self::StreamIdle => 0,
            Self::StreamTransport => 1_000,
            Self::ConnectionPoolExhausted => 5_000,
            Self::Network => 3_000,
            _ => return None,
        };
        Some(base * (1u64 << attempt.min(3)))
    }

    /// Actionable guidance for humans and LLMs.
    ///
    /// This is the single source of truth for "what happened and what to do next".
    /// Tool-specific guidance (alternatives, scope narrowing) is layered on top
    /// by the caller.
    #[must_use]
    pub fn guidance(self) -> &'static str {
        match self {
            Self::RateLimit => {
                "Rate limit hit. The system will retry automatically. \
                 Reduce parallel tool calls if this persists."
            }
            Self::ServerError => {
                "LLM provider returned a server error (5xx). \
                 Retrying automatically with backoff."
            }
            Self::Auth => {
                "Authentication failed (401/403). \
                 Do NOT retry — credentials need to be refreshed."
            }
            Self::ContextWindow => {
                "Prompt exceeds the model's context window. \
                 Reduce input size: drop older messages, summarize, or use a model with a larger context."
            }
            Self::InvalidRequest => {
                "The request was rejected by the LLM provider (400). \
                 This may indicate a bug in request assembly. Do NOT retry with the same parameters."
            }
            Self::StreamIdle => {
                "Model stopped sending tokens mid-stream (idle timeout). \
                 Retrying the same request. If this recurs, try a different model or reduce input size."
            }
            Self::StreamTransport => {
                "Connection to the LLM provider was lost mid-stream. \
                 Retrying automatically."
            }
            Self::BudgetExhausted => {
                "Turn/session time budget exhausted. \
                 Wrap up with what you have or ask the user to extend the budget."
            }
            Self::ToolRoundsExhausted => {
                "Maximum tool rounds reached for this turn. \
                 Provide your best answer with the information gathered so far."
            }
            Self::Network => {
                "Network error (DNS, connection refused, unreachable). \
                 Retrying with backoff. Check network connectivity if this persists."
            }
            Self::ToolNotFound => {
                "Tool or resource not found. \
                 Verify the name/path is correct before retrying."
            }
            Self::ToolInvalidArgs => {
                "Invalid arguments for the tool. \
                 Fix the parameters and retry the same tool before switching tools."
            }
            Self::ToolTimeout => {
                "Tool command timed out. Do NOT retry with the same arguments. \
                 Run a narrower target, increase timeout only for commands expected to be slow, \
                 or split the work into focused commands."
            }
            Self::ToolUnavailable => {
                "Tool is not available in this environment. \
                 Do NOT retry — use an alternative tool."
            }
            Self::ToolBinding => {
                "Tool execution binding is missing for this turn. \
                 Do NOT retry the same tool call and do NOT assume a shell command \
                 is equivalent; continue degraded only after making the lost capability explicit."
            }
            Self::ResourceLimit => {
                "System resource limit reached (memory/disk/processes). \
                 This tool is BLOCKED for the rest of this session. \
                 Reduce system load or try a different approach."
            }
            Self::ConnectionPoolExhausted => {
                "HTTP connection pool saturated (reqwest/hyper). \
                 Reduce parallel LLM requests, check for response body leaks, \
                 or increase pool_max_size. Retrying with backoff."
            }
            Self::DatabaseError => {
                "Database query failed (SQL syntax, deadlock, or pool). \
                 Do NOT retry with the same query. Inspect the error and \
                 adjust the query or schema usage."
            }
            Self::Stall => {
                "Agent loop detected — same action repeated without progress. \
                 Do NOT repeat the last action. Try a different tool, widen the \
                 context, or hand control back to the user."
            }
            Self::MissingModelSelection => {
                "No concrete model was selected. Do NOT choose a fallback model. \
                 Ask the user to select a model or set a CLI default_model."
            }
            Self::Cancelled => "Operation was cancelled by the user.",
            Self::Unknown => "An unexpected error occurred. Check the error output and adjust.",
        }
    }

    /// Operator-facing remediation hint (distinct from [`Self::guidance`]).
    ///
    /// `guidance` is for the LLM — tells the model what to *do* next inside
    /// the turn. `diagnosis_hint` is for the human operator reviewing a
    /// session postmortem — tells them what *system-level fix* to apply
    /// (update config, re-login, raise ulimits, switch model, etc.).
    #[must_use]
    pub fn diagnosis_hint(self) -> &'static str {
        match self {
            Self::RateLimit => "Reduce parallel tool calls or raise the provider rate-limit quota.",
            Self::ServerError => {
                "Transient provider issue. If it persists, switch model or provider."
            }
            Self::Auth => {
                "Re-authenticate with `/login`. Check token expiry. \
                 Verify API credentials in environment variables."
            }
            Self::ContextWindow => {
                "Prompt is too large. Compact history, trim tool schemas, \
                 or switch to a larger-context model."
            }
            Self::InvalidRequest => {
                "Request assembly bug or stale tool schema. Inspect the provider \
                 payload and fix at the source — do not retry blindly."
            }
            Self::StreamIdle => {
                "Model stalled mid-stream. If recurring, switch model or reduce input size."
            }
            Self::StreamTransport => {
                "Transport dropped mid-stream. Check network stability; if persistent, \
                 change model endpoint or provider."
            }
            Self::BudgetExhausted => {
                "Extend the turn/session budget, or break the task into smaller goals."
            }
            Self::ToolRoundsExhausted => {
                "Raise the per-turn tool-round cap, or restructure the task so fewer \
                 tool calls are needed."
            }
            Self::Network => {
                "Check network connectivity and proxy settings. Verify \
                 `NO_PROXY=localhost,127.0.0.1` for local services. \
                 Confirm target service is running."
            }
            Self::ToolNotFound => {
                "Agent guessed wrong paths. Use `list_dir` before `read_file`/`grep`. \
                 Confirm the workspace context is accurate."
            }
            Self::ToolInvalidArgs => {
                "Model is calling tools with wrong parameters. Improve schema/description \
                 clarity and steer the model to retry the same tool with corrected args \
                 instead of escalating to bash or unrelated tools."
            }
            Self::ToolTimeout => {
                "Tool scope is too broad. Break the operation into smaller chunks \
                 or raise the tool timeout if genuinely long-running."
            }
            Self::ToolUnavailable => {
                "Install or configure the missing tool, or remove it from the skill \
                 manifest so the agent does not pick it."
            }
            Self::ToolBinding => {
                "Fix the tool-surface/executor mismatch: only advertise tools whose \
                 executor or edge transport is bound for the turn, or wire the \
                 missing transport before activation."
            }
            Self::ResourceLimit => {
                "Check system limits: `ulimit -u` (max procs), `ulimit -n` (open files). \
                 Kill orphan processes: `ps aux | grep defunct`. May need to restart \
                 the system or increase limits."
            }
            Self::ConnectionPoolExhausted => {
                "The reqwest HTTP pool is saturated. Reduce parallel LLM calls, \
                 check for un-consumed SSE response bodies, and verify \
                 `pool_max_idle_per_host` / `pool_max_size` settings."
            }
            Self::DatabaseError => {
                "Check MatrixOne connectivity and SQL syntax. Use CAST for DATETIME \
                 columns, MIN/MAX for non-grouped columns. For deadlocks, reorder \
                 transactions or shrink their scope."
            }
            Self::Stall => {
                "Agent is stuck in a loop. Try `/rewind` to go back, or switch to \
                 a different model with `/model`. Break complex tasks into smaller steps."
            }
            Self::MissingModelSelection => {
                "Select a concrete model with `/model set <name>`, pass `--model <name>`, \
                 or run `astra config set default_model <name>`."
            }
            Self::Cancelled => {
                "User cancelled — no action needed unless cancellations are unexpected."
            }
            Self::Unknown => {
                "Review the error samples to identify the pattern, then add a \
                 classifier rule in `classify_tool_output`."
            }
        }
    }
}

impl std::fmt::Display for ErrorKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl ErrorKind {
    /// Parse from the stable string tag produced by [`as_str`].
    #[must_use]
    pub fn parse_tag(s: &str) -> Option<Self> {
        match s {
            "rate_limit" => Some(Self::RateLimit),
            "server_error" => Some(Self::ServerError),
            "auth" => Some(Self::Auth),
            "context_window" => Some(Self::ContextWindow),
            "invalid_request" => Some(Self::InvalidRequest),
            "stream_idle" => Some(Self::StreamIdle),
            "stream_transport" => Some(Self::StreamTransport),
            "connection_pool_exhausted" => Some(Self::ConnectionPoolExhausted),
            "budget_exhausted" => Some(Self::BudgetExhausted),
            "tool_rounds_exhausted" => Some(Self::ToolRoundsExhausted),
            "network" => Some(Self::Network),
            "tool_not_found" => Some(Self::ToolNotFound),
            "tool_invalid_args" => Some(Self::ToolInvalidArgs),
            "tool_timeout" => Some(Self::ToolTimeout),
            "tool_unavailable" => Some(Self::ToolUnavailable),
            "tool_binding" => Some(Self::ToolBinding),
            "resource_limit" => Some(Self::ResourceLimit),
            "database_error" => Some(Self::DatabaseError),
            "stall" => Some(Self::Stall),
            "missing_model_selection" => Some(Self::MissingModelSelection),
            "cancelled" => Some(Self::Cancelled),
            "unknown" => Some(Self::Unknown),
            _ => None,
        }
    }
}

// ── ClassifiedError ──────────────────────────────────────────────────────────

/// An error with its classification attached at the source.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClassifiedError {
    pub kind: ErrorKind,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details_json: Option<String>,
}

impl ClassifiedError {
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            details_json: None,
        }
    }

    #[must_use]
    pub fn with_details_json(mut self, details_json: impl Into<String>) -> Self {
        self.details_json = Some(details_json.into());
        self
    }

    /// Structured feedback suitable for appending to conversation history.
    /// Combines the error message with actionable guidance.
    #[must_use]
    pub fn llm_feedback(&self) -> String {
        format!(
            "[{}] {}\n→ {}",
            self.kind.as_str(),
            self.message,
            self.kind.guidance()
        )
    }
}

impl std::fmt::Display for ClassifiedError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "[{}] {}", self.kind.as_str(), self.message)
    }
}

impl std::error::Error for ClassifiedError {}

impl From<String> for ClassifiedError {
    fn from(s: String) -> Self {
        // Try to recover ErrorKind from the [kind] prefix produced by Display.
        if let Some(rest) = s.strip_prefix('[')
            && let Some(bracket_end) = rest.find(']')
        {
            let tag = &rest[..bracket_end];
            if let Some(kind) = ErrorKind::parse_tag(tag) {
                let message = rest[bracket_end + 1..].trim_start().to_string();
                return Self {
                    kind,
                    message,
                    details_json: None,
                };
            }
        }
        let kind = crate::classify_tool_output(&s);
        Self {
            kind,
            message: s,
            details_json: None,
        }
    }
}

// ── LLM error-message fallback classifier ────────────────────────────────────

/// Boundary fallback for context-window / prompt-too-long provider text.
///
/// This is not a general structured error classifier. Callers that already know
/// the failure kind should construct [`ErrorKind::ContextWindow`] directly.
#[must_use]
pub fn is_llm_context_window_error(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    is_unstructured_llm_context_window_message_lower(&lower)
}

fn is_unstructured_llm_context_window_message_lower(lower: &str) -> bool {
    lower.contains("context_length_exceeded")
        || lower.contains("maximum context length")
        || lower.contains("prompt is too long")
        || lower.contains("too many tokens")
        || lower.contains("input is too long")
        || lower.contains("context window")
        || is_max_tokens_context_limit_error(lower)
}

fn is_max_tokens_context_limit_error(lower: &str) -> bool {
    lower.contains("max_tokens")
        && (lower.contains("max_tokens limit")
            || lower.contains("max_tokens exceeded")
            || lower.contains("max_tokens is too large")
            || lower.contains("max_tokens exceeds")
            || ((lower.contains("context") || lower.contains("prompt"))
                && (lower.contains("exceed") || lower.contains("too long"))))
}

fn is_rate_limit_error_lower(lower: &str) -> bool {
    lower.contains("rate limit") || lower.contains("429") || lower.contains("too many requests")
}

fn is_database_error_lower(lower: &str) -> bool {
    lower.contains("db query:")
        || lower.contains("database operation failed")
        || lower.contains("error communicating with database")
        || lower.contains("error returned from database")
        || lower.contains("sql syntax error")
        || lower.contains("sqlx")
        || lower.contains("deadlock")
        || lower.contains("matrixone pool timed out")
        || lower.contains("duplicate entry")
        || lower.contains("foreign key constraint")
        || lower.contains("invalid infra_llm_models.")
        || lower.contains("invalid infra_user_config.")
        || lower.contains("invalid agent_runs.")
        || lower.contains("invalid agent_sessions.")
        || lower.contains("connection pool timed out")
}

/// Classify an unstructured LLM provider / transport error message.
///
/// This is the single fallback for LLM calls that cross a boundary without
/// structured error metadata. New call sites should still prefer constructing
/// [`ClassifiedError`] at the source.
#[must_use]
pub fn classify_llm_error_message(message: &str) -> ErrorKind {
    let lower = message.to_ascii_lowercase();
    if lower.contains("model selection is required")
        || lower.contains("missing model selection")
        || lower.contains("no concrete model was selected")
    {
        return ErrorKind::MissingModelSelection;
    }
    if is_rate_limit_error_lower(&lower) {
        ErrorKind::RateLimit
    } else if is_connection_pool_timeout(&lower) {
        ErrorKind::ConnectionPoolExhausted
    } else if is_unstructured_llm_context_window_message_lower(&lower) {
        ErrorKind::ContextWindow
    } else if lower.contains("timeout") || lower.contains("timed out") {
        ErrorKind::StreamIdle
    } else if lower.contains("connect") || lower.contains("transport") || lower.contains("network")
    {
        ErrorKind::StreamTransport
    } else if lower.contains("401 unauthorized")
        || lower.contains("status: 401")
        || lower.contains("status code: 401")
        || lower.contains("http 401")
        || lower.contains("unauthorized")
        || lower.contains("api key")
        || lower.contains("could not validate credentials")
        || lower.contains("invalid credentials")
        || lower.contains("bad credentials")
        || lower.contains("authentication failed")
        || lower.contains("token expired")
        || lower.contains("invalid token")
        || lower.contains("security token included in the request is expired")
    {
        ErrorKind::Auth
    } else if lower.contains("cancelled") || lower.contains("canceled") {
        ErrorKind::Cancelled
    } else {
        ErrorKind::Unknown
    }
}

/// Classify model registry / model-selection resolution failures.
///
/// This boundary is neither an LLM provider failure nor a tool failure. Keep it
/// separate so database outages, missing model selection, and invalid model
/// overrides do not collapse into `unknown` or tool-oriented categories.
#[must_use]
pub fn classify_model_resolution_error_message(message: &str) -> ErrorKind {
    let lower = message.to_ascii_lowercase();
    if lower.contains("model selection is required")
        || lower.contains("missing model selection")
        || lower.contains("no concrete model was selected")
    {
        return ErrorKind::MissingModelSelection;
    }
    if is_database_error_lower(&lower) {
        return ErrorKind::DatabaseError;
    }
    if lower.contains("not configured on this server")
        || lower.contains("no exact or substring match in infra_llm_models")
        || lower.contains("registered active models")
        || lower.contains("is inactive")
        || lower.contains(" is ambiguous")
        || lower.contains("empty model name")
    {
        return ErrorKind::InvalidRequest;
    }
    ErrorKind::Unknown
}

// ── Tool output fallback classifier ──────────────────────────────────────────

/// External tool-output fallback classifier.
///
/// Used exclusively for external tool output (bash, MCP tools) where we don't
/// control the error format. LLM provider / transport strings use
/// [`classify_llm_error_message`]; internal runtime errors should be constructed
/// with the correct [`ErrorKind`] at their source.
///
/// For external categories, more specific patterns come first:
/// ResourceLimit > DatabaseError > ToolTimeout > Network > Auth >
/// ToolInvalidArgs(call contract) > ToolUnavailable >
/// ToolInvalidArgs(generic) > ToolNotFound > Unknown.
#[must_use]
pub fn classify_tool_output(error_str: &str) -> ErrorKind {
    let lower = error_str.to_lowercase();

    if lower.contains("model selection is required")
        || lower.contains("missing model selection")
        || lower.contains("no concrete model was selected")
    {
        return ErrorKind::MissingModelSelection;
    }

    // Resource limit — never retry, block the tool
    if lower.contains("resource temporarily unavailable")
        || lower.contains("cannot allocate memory")
        || lower.contains("out of memory")
        || lower.contains("no space left on device")
        || lower.contains("too many open files")
        || lower.contains("fork:")
        || lower.contains("enomem")
        || lower.contains("enospc")
        || lower.contains("ebusy")
        || lower.contains("emfile")
        || lower.contains("device or resource busy")
        || contains_word(&lower, "oom")
        || lower.contains("oom-killer")
        || lower.contains("oom killer")
        || lower.contains("资源暂时不足")
        || lower.contains("系统资源")
        || lower.contains("内存不足")
    {
        return ErrorKind::ResourceLimit;
    }

    // Workspace read-before-write — classify as invalid args
    if is_workspace_read_before_write(&lower) {
        return ErrorKind::ToolInvalidArgs;
    }

    // Database errors — MatrixOne / SQLx. Must come before Network because
    // "connection pool timed out" reads like a network issue but the fix
    // lives in the DB layer (pool config, slow queries, deadlocks).
    if is_database_error_lower(&lower) || (lower.contains("column") && lower.contains("group by")) {
        return ErrorKind::DatabaseError;
    }

    // Local command timeout — different from network timeout
    if lower.contains("command timed out")
        || lower.contains("grep timed out")
        || lower.contains("deadline exceeded")
        || (lower.contains("timed out after") && !lower.contains("connection"))
    {
        return ErrorKind::ToolTimeout;
    }

    // Transient: network, timeout, rate limit, server errors.
    // HTTP status codes require "http" context or textual description to
    // avoid false positives on line numbers / filenames containing "500".
    if lower.contains("timeout")
        || lower.contains("timed out")
        || lower.contains("rate limit")
        || lower.contains("too many requests")
        || lower.contains("http 429")
        || lower.contains("status 429")
        || (lower.contains("429") && lower.contains("rate"))
        || lower.contains("http 500")
        || lower.contains("http 502")
        || lower.contains("http 503")
        || lower.contains("http 504")
        || lower.contains("status 500")
        || lower.contains("status 502")
        || lower.contains("status 503")
        || lower.contains("status 504")
        || lower.contains("internal server error")
        || lower.contains("service unavailable")
        || lower.contains("bad gateway")
        || lower.contains("connection reset")
        || lower.contains("connection refused")
        || lower.contains("connection closed")
        || lower.contains("econnreset")
        || lower.contains("econnrefused")
        || lower.contains("etimedout")
        || lower.contains("epipe")
        || lower.contains("broken pipe")
        || lower.contains("network")
        || lower.contains("dns")
        || lower.contains("error sending request")
        || lower.contains("deadline exceeded")
    {
        return ErrorKind::Network;
    }

    // Auth — numeric codes require "http" context to avoid false positives.
    if lower.contains("http 401")
        || lower.contains("status 401")
        || lower.contains("http 403")
        || lower.contains("status 403")
        || lower.contains("unauthorized")
        || lower.contains("forbidden")
        || lower.contains("permission denied")
        || lower.contains("access denied")
        || lower.contains("authentication")
        || lower.contains("auth failed")
        || lower.contains("token expired")
        || lower.contains("invalid token")
        || lower.contains("credentials")
        || lower.contains("could not validate")
        || lower.contains("eacces")
        || lower.contains("eperm")
        || lower.contains("operation not permitted")
    {
        return ErrorKind::Auth;
    }

    // Tool-call contract errors are caller-fixable: wrong field, wrong
    // parameter combination, or a tool called outside the current turn's
    // advertised tool contract. They must not quarantine the tool as
    // unavailable; the same tool can succeed with corrected arguments.
    if is_tool_call_contract_error(&lower) {
        return ErrorKind::ToolInvalidArgs;
    }

    // Binding / protocol mismatches are surface-assembly bugs: the model was
    // shown a tool whose executor was not actually attached. Record the error
    // pressure but do not teach tool health that the tool itself is flaky.
    if lower.contains("binding failure")
        || lower.contains("tool binding")
        || lower.contains("executor not bound")
        || lower.contains("no executor attached")
        || (lower.contains("not bound")
            && (lower.contains("executor") || lower.contains("tool") || lower.contains("binding")))
    {
        return ErrorKind::ToolBinding;
    }

    // Unavailable — check BEFORE "not found" because "command not found" should
    // be ToolUnavailable, not ToolNotFound.
    if lower.contains("not installed")
        || lower.contains("not available")
        || lower.contains("not configured")
        || lower.contains("command not found")
        || lower.contains("no such command")
        || lower.contains("not supported")
        || lower.contains("not implemented")
        || lower.contains("unavailable")
    {
        return ErrorKind::ToolUnavailable;
    }

    // Invalid args — check BEFORE "not found" because "old_str not found" is
    // a tool misuse, not a missing file.
    if lower.contains("invalid argument")
        || lower.contains("invalid parameter")
        || lower.contains("missing required")
        || lower.contains("missing '")
        || lower.contains("parse error")
        || lower.contains("syntax error")
        || lower.contains("unexpected token")
        || lower.contains("unexpected argument")
        || lower.contains("unrecognized option")
        || lower.contains("type mismatch")
        || lower.contains("invalid json")
        || lower.contains("malformed")
        || lower.contains("file is too large")
        || lower.contains("old_str not found")
        || lower.contains("str_replace failed")
        || lower.contains("sandbox")
        || lower.contains("unsupported")
    {
        return ErrorKind::ToolInvalidArgs;
    }

    // Not found
    if lower.contains("not found")
        || lower.contains("no such file")
        || lower.contains("does not exist")
        || lower.contains("enoent")
        || lower.contains("404")
        || lower.contains("couldn't find")
        || lower.contains("unknown tool")
        || lower.contains("is a directory")
        || lower.contains("eisdir")
    {
        return ErrorKind::ToolNotFound;
    }

    ErrorKind::Unknown
}

fn is_tool_call_contract_error(lower: &str) -> bool {
    if lower.contains("not available in this turn")
        && (lower.contains("tools[]")
            || lower.contains("visible")
            || lower.contains("deferred_tools")
            || lower.contains("tool_search"))
    {
        return true;
    }

    if lower.contains("unknown field")
        || lower.contains("valid fields")
        || lower.contains("required field")
        || lower.contains("unsupported field")
    {
        return true;
    }

    let contract_violation = lower.contains("unsupported")
        || lower.contains("not supported")
        || lower.contains("only supports")
        || lower.contains("only accepts")
        || lower.contains("unknown")
        || lower.contains("missing")
        || lower.contains("invalid")
        || lower.contains("malformed")
        || lower.contains("unexpected")
        || lower.contains("required")
        || lower.contains("must be")
        || lower.contains("mutually exclusive")
        || lower.contains("cannot be used with");

    contract_violation && has_tool_call_contract_subject(lower)
}

fn has_tool_call_contract_subject(lower: &str) -> bool {
    [
        "field",
        "argument",
        "arg",
        "parameter",
        "param",
        "property",
        "option",
        "flag",
        "key",
        "value",
        "payload",
        "schema",
        "input",
        "json",
        "action",
        "status",
        "mode",
        "format",
        "enum",
        "type",
    ]
    .iter()
    .any(|word| contains_word(lower, word))
}

/// True when `word` appears as a standalone word in `haystack` (already
/// lowercased). A "word boundary" is start/end of string or any non-alphanumeric
/// character. This prevents "oom" from matching inside "room" or "zoom".
fn contains_word(haystack: &str, word: &str) -> bool {
    let haystack_bytes = haystack.as_bytes();
    let word_bytes = word.as_bytes();
    let wlen = word_bytes.len();
    if wlen == 0 || haystack_bytes.len() < wlen {
        return false;
    }
    let mut start = 0;
    while let Some(pos) = haystack[start..].find(word) {
        let abs = start + pos;
        let before_ok = abs == 0 || !haystack_bytes[abs - 1].is_ascii_alphanumeric();
        let after_ok = abs + wlen >= haystack_bytes.len()
            || !haystack_bytes[abs + wlen].is_ascii_alphanumeric();
        if before_ok && after_ok {
            return true;
        }
        start = abs + 1;
    }
    false
}

/// True when the error comes from the Edge workspace guard: existing paths must
/// be read before overwrite/patch.
pub fn is_workspace_read_before_write(lower: &str) -> bool {
    lower.contains("not been read yet")
        || lower.contains("read it first before")
        || lower.contains("only partially read")
        || (lower.contains("partially read") && lower.contains("write"))
        || lower.contains("modified since last read")
        || lower.contains("read it again before")
        || lower.contains("read the full file before overwriting")
}

fn is_connection_pool_timeout(lower: &str) -> bool {
    lower.contains("connection pool timed out")
        || lower.contains("pool timed out while waiting for an open connection")
        || lower.contains("pool timed out waiting for an open connection")
        || (lower.contains("pool timed out") && lower.contains("open connection"))
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── ErrorKind basics ──

    /// Every ErrorKind variant. Keep in sync when adding new variants.
    const ALL_VARIANTS: &[ErrorKind] = &[
        ErrorKind::RateLimit,
        ErrorKind::ServerError,
        ErrorKind::Auth,
        ErrorKind::ContextWindow,
        ErrorKind::InvalidRequest,
        ErrorKind::StreamIdle,
        ErrorKind::StreamTransport,
        ErrorKind::ConnectionPoolExhausted,
        ErrorKind::BudgetExhausted,
        ErrorKind::ToolRoundsExhausted,
        ErrorKind::Network,
        ErrorKind::ToolNotFound,
        ErrorKind::ToolInvalidArgs,
        ErrorKind::ToolTimeout,
        ErrorKind::ToolUnavailable,
        ErrorKind::ToolBinding,
        ErrorKind::ResourceLimit,
        ErrorKind::DatabaseError,
        ErrorKind::Stall,
        ErrorKind::MissingModelSelection,
        ErrorKind::Cancelled,
        ErrorKind::Unknown,
    ];

    #[test]
    fn error_kind_serde_and_exhaustiveness() {
        // Serialization roundtrip for ALL_VARIANTS
        for &kind in ALL_VARIANTS {
            let json = serde_json::to_string(&kind).unwrap();
            let back: ErrorKind = serde_json::from_str(&json).unwrap();
            assert_eq!(kind, back, "roundtrip failed for {kind:?}");
        }
        // ALL_VARIANTS must have no duplicates
        let tags: std::collections::HashSet<&str> =
            ALL_VARIANTS.iter().map(|k| k.as_str()).collect();
        assert_eq!(
            tags.len(),
            ALL_VARIANTS.len(),
            "ALL_VARIANTS has duplicates"
        );
        // parse_tag must round-trip
        for &kind in ALL_VARIANTS {
            assert_eq!(ErrorKind::parse_tag(kind.as_str()), Some(kind));
        }
        assert_eq!(ErrorKind::parse_tag("nonexistent"), None);
    }

    #[test]
    fn error_kind_meta_properties() {
        for &kind in ALL_VARIANTS {
            assert!(!kind.guidance().is_empty(), "empty guidance for {kind:?}");
            assert!(!kind.as_str().is_empty(), "empty as_str for {kind:?}");
            assert!(
                !kind.diagnosis_hint().is_empty(),
                "empty diagnosis_hint for {kind:?}"
            );
        }
        // display matches as_str
        for kind in [ErrorKind::RateLimit, ErrorKind::Auth, ErrorKind::Unknown] {
            assert_eq!(format!("{kind}"), kind.as_str());
        }
    }

    #[test]
    fn retry_behavior() {
        // Known retryable
        for kind in [
            ErrorKind::RateLimit,
            ErrorKind::ServerError,
            ErrorKind::StreamIdle,
            ErrorKind::StreamTransport,
            ErrorKind::Network,
        ] {
            assert!(kind.is_retryable(), "{kind:?} must be retryable");
        }
        // Known non-retryable
        for kind in [
            ErrorKind::Auth,
            ErrorKind::ContextWindow,
            ErrorKind::Cancelled,
            ErrorKind::ResourceLimit,
            ErrorKind::ToolTimeout,
            ErrorKind::ToolBinding,
            ErrorKind::MissingModelSelection,
        ] {
            assert!(!kind.is_retryable(), "{kind:?} must NOT be retryable");
        }
        // Exponential backoff for RateLimit
        assert_eq!(ErrorKind::RateLimit.retry_delay_ms(0), Some(5_000));
        assert_eq!(ErrorKind::RateLimit.retry_delay_ms(1), Some(10_000));
        assert_eq!(ErrorKind::RateLimit.retry_delay_ms(2), Some(20_000));
        assert_eq!(ErrorKind::RateLimit.retry_delay_ms(3), Some(40_000));
        assert_eq!(ErrorKind::RateLimit.retry_delay_ms(10), Some(40_000));
        // Other retry delay behaviors
        assert_eq!(ErrorKind::StreamIdle.retry_delay_ms(0), Some(0));
        assert_eq!(ErrorKind::Auth.retry_delay_ms(0), None);
        // Invariant: delay ↔ retryable for all variants
        for &kind in ALL_VARIANTS {
            match kind.retry_delay_ms(0) {
                Some(_) => assert!(
                    kind.is_retryable(),
                    "non-retryable {kind:?} returned a delay"
                ),
                None => assert!(!kind.is_retryable(), "retryable {kind:?} returned no delay"),
            }
        }
    }

    // ── ClassifiedError ──

    #[test]
    fn classified_error_display_and_feedback() {
        // Display: classified error to_string format
        let err = ClassifiedError::new(ErrorKind::Auth, "401 Unauthorized");
        assert_eq!(err.to_string(), "[auth] 401 Unauthorized");
        // Display with empty message
        let empty = ClassifiedError::new(ErrorKind::Unknown, "");
        assert_eq!(empty.to_string(), "[unknown] ");
        // llm_feedback has tag, message, and arrow
        let fb = ClassifiedError::new(ErrorKind::StreamIdle, "no chunk in 90000ms").llm_feedback();
        assert!(fb.starts_with("[stream_idle]"));
        assert!(fb.contains("no chunk in 90000ms"));
        assert!(fb.contains("→"));
        // llm_feedback for empty tag
        let fb_empty = empty.llm_feedback();
        assert!(fb_empty.contains("[unknown]"));
        assert!(fb_empty.contains("→"));
    }

    #[test]
    fn classified_error_from_string_roundtrip() {
        // Recover kind from [prefix] in Display string
        for kind in [
            ErrorKind::RateLimit,
            ErrorKind::ServerError,
            ErrorKind::Auth,
            ErrorKind::ContextWindow,
            ErrorKind::InvalidRequest,
            ErrorKind::StreamIdle,
            ErrorKind::BudgetExhausted,
            ErrorKind::Cancelled,
        ] {
            let original = ClassifiedError::new(kind, "test");
            let roundtrip = ClassifiedError::from(original.to_string());
            assert_eq!(roundtrip.kind, kind, "round-trip failed for {kind:?}");
        }
        // Also check message preservation
        let err = ClassifiedError::new(ErrorKind::RateLimit, "429 too many requests");
        let rt = ClassifiedError::from(err.to_string());
        assert_eq!(rt.message, "429 too many requests");
    }

    #[test]
    fn from_string_no_prefix_falls_back_to_classify() {
        let err = ClassifiedError::from("connection refused".to_string());
        assert_eq!(err.kind, ErrorKind::Network);
        assert_eq!(err.message, "connection refused");
    }

    #[test]
    fn classified_error_is_std_error() {
        let err = ClassifiedError::new(ErrorKind::Auth, "bad key");
        let _: &dyn std::error::Error = &err;
    }

    // ── classify_llm_error_message ──

    #[test]
    fn classify_llm_error_message_cases() {
        let cases: &[(&str, ErrorKind)] = &[
            (
                "model selection is required",
                ErrorKind::MissingModelSelection,
            ),
            (
                "missing model selection for tool",
                ErrorKind::MissingModelSelection,
            ),
            (
                "no concrete model was selected",
                ErrorKind::MissingModelSelection,
            ),
            ("context_length_exceeded", ErrorKind::ContextWindow),
            ("maximum context length is 128000", ErrorKind::ContextWindow),
            ("prompt is too long", ErrorKind::ContextWindow),
            ("too many tokens in the input", ErrorKind::ContextWindow),
            ("input is too long for this model", ErrorKind::ContextWindow),
            ("context window exceeded", ErrorKind::ContextWindow),
            ("max_tokens limit exceeded", ErrorKind::ContextWindow),
            ("rate limit exceeded", ErrorKind::RateLimit),
            ("error 429: too many requests", ErrorKind::RateLimit),
            (
                "Error: pool timed out while waiting for an open connection",
                ErrorKind::ConnectionPoolExhausted,
            ),
            (
                "rate limit exceeded for max_tokens setting",
                ErrorKind::RateLimit,
            ),
            (
                "rate limit exceeded while pool timed out waiting for an open connection",
                ErrorKind::RateLimit,
            ),
            ("request timed out", ErrorKind::StreamIdle),
            ("connection refused", ErrorKind::StreamTransport),
            (
                "LLM stream transport error: connection reset",
                ErrorKind::StreamTransport,
            ),
            ("401 unauthorized", ErrorKind::Auth),
            ("Error: Could not validate credentials", ErrorKind::Auth),
            ("invalid credentials", ErrorKind::Auth),
            ("bad credentials", ErrorKind::Auth),
            ("token expired", ErrorKind::Auth),
            ("invalid token", ErrorKind::Auth),
            ("authentication failed", ErrorKind::Auth),
            (
                "The security token included in the request is expired",
                ErrorKind::Auth,
            ),
            ("LLM call cancelled", ErrorKind::Cancelled),
            ("something went wrong", ErrorKind::Unknown),
        ];

        for (input, expected) in cases {
            assert_eq!(
                classify_llm_error_message(input),
                *expected,
                "classify_llm_error_message({input:?}) should be {expected:?}"
            );
        }
    }

    #[test]
    fn is_llm_context_window_error_is_case_insensitive() {
        assert!(is_llm_context_window_error("MAX_TOKENS LIMIT EXCEEDED"));
        assert!(!is_llm_context_window_error("rate limit exceeded"));
        assert!(!is_llm_context_window_error("internal server error"));
        assert!(!is_llm_context_window_error(""));
    }

    #[test]
    fn classify_model_resolution_error_message_cases() {
        let cases: &[(&str, ErrorKind)] = &[
            (
                "Model resolution failed: DB query: error communicating with database: expected to read 4 bytes, got 0 bytes at EOF",
                ErrorKind::DatabaseError,
            ),
            (
                "Model resolution failed: Model selection is required. Select a concrete model with `/model set <name>`.",
                ErrorKind::MissingModelSelection,
            ),
            (
                "Model resolution failed: Model 'foo' is not configured on this server (no exact or substring match in infra_llm_models). Registered active models: []",
                ErrorKind::InvalidRequest,
            ),
            (
                "Model resolution failed: Model 'foo' is inactive (connectivity failed or disabled).",
                ErrorKind::InvalidRequest,
            ),
            (
                "Model resolution failed: Model 'gpt' is ambiguous -- matches multiple registered models",
                ErrorKind::InvalidRequest,
            ),
            (
                "Model resolution failed: something novel",
                ErrorKind::Unknown,
            ),
        ];

        for &(input, expected) in cases {
            assert_eq!(
                classify_model_resolution_error_message(input),
                expected,
                "classify_model_resolution_error_message({input:?}) should be {expected:?}"
            );
        }
    }

    // ── classify_tool_output ──

    #[test]
    fn classify_tool_output_cases() {
        let cases: &[(&str, ErrorKind)] = &[
            // ResourceLimit
            (
                "fork: Resource temporarily unavailable",
                ErrorKind::ResourceLimit,
            ),
            ("Cannot allocate memory", ErrorKind::ResourceLimit),
            ("系统资源不足，无法完成操作", ErrorKind::ResourceLimit),
            ("内存不足", ErrorKind::ResourceLimit),
            // ToolTimeout
            ("command timed out after 30s", ErrorKind::ToolTimeout),
            ("grep timed out", ErrorKind::ToolTimeout),
            // Network
            ("connection timed out after 30s", ErrorKind::Network),
            ("ETIMEDOUT", ErrorKind::Network),
            ("HTTP 503 Service Unavailable", ErrorKind::Network),
            // Database
            (
                "DB query: error communicating with database: expected to read 4 bytes, got 0 bytes at EOF",
                ErrorKind::DatabaseError,
            ),
            (
                "database operation failed: operation=load_run_metadata_for_user, source=connection pool timed out",
                ErrorKind::DatabaseError,
            ),
            // Auth
            ("401 Unauthorized", ErrorKind::Auth),
            ("Permission denied: insufficient scope", ErrorKind::Auth),
            ("EACCES: permission denied", ErrorKind::Auth),
            ("EPERM: operation not permitted", ErrorKind::Auth),
            // ToolNotFound
            ("No such file or directory", ErrorKind::ToolNotFound),
            ("ENOENT: file does not exist", ErrorKind::ToolNotFound),
            ("Is a directory", ErrorKind::ToolNotFound),
            (
                "EISDIR: illegal operation on a directory",
                ErrorKind::ToolNotFound,
            ),
            // ToolInvalidArgs
            (
                "invalid argument: expected integer",
                ErrorKind::ToolInvalidArgs,
            ),
            (
                "File has not been read yet — read it first before editing",
                ErrorKind::ToolInvalidArgs,
            ),
            (
                "File was only partially read; read the full file before overwriting",
                ErrorKind::ToolInvalidArgs,
            ),
            (
                "Error: file is too large (97716 bytes)",
                ErrorKind::ToolInvalidArgs,
            ),
            (
                "Error: unknown field `offset` for read_file. Valid fields: path, start_line, end_line, outline. Required: path. `read_file` uses line numbers, not byte offsets; use `start_line`.",
                ErrorKind::ToolInvalidArgs,
            ),
            (
                "Error: field 'subtask_id' only supports new_status updates; unsupported with subtask_id: reason",
                ErrorKind::ToolInvalidArgs,
            ),
            (
                "Error: unsupported output_mode 'xml'. Use 'content', 'files_with_matches', or 'count'.",
                ErrorKind::ToolInvalidArgs,
            ),
            (
                "error: unexpected argument 'edge_tools::tests::run_chain' found\n\nUsage: cargo test [OPTIONS] [TESTNAME]",
                ErrorKind::ToolInvalidArgs,
            ),
            (
                "Error: Tool 'task' is not available in this turn. Call only tools visible in this turn's `tools[]`.",
                ErrorKind::ToolInvalidArgs,
            ),
            (
                "Error: Tool 'agent_fanout' is not available in this turn yet. It appears in `<deferred-tools>`, so first call `tool_search`.",
                ErrorKind::ToolInvalidArgs,
            ),
            // ToolUnavailable
            ("command not found: rg", ErrorKind::ToolUnavailable),
            ("bash: rg: command not found", ErrorKind::ToolUnavailable),
            (
                "run_script is not available on this platform (requires Unix domain sockets)",
                ErrorKind::ToolUnavailable,
            ),
            (
                "restore_snapshot_state is not supported for this store",
                ErrorKind::ToolUnavailable,
            ),
            // ToolBinding
            (
                "binding failure: tool `reflect` has no executor",
                ErrorKind::ToolBinding,
            ),
            (
                "tool binding mismatch: requested introspect executor not bound",
                ErrorKind::ToolBinding,
            ),
            (
                "Error: tool binding failed for agent_fanout: no executor",
                ErrorKind::ToolBinding,
            ),
            // "not available" still belongs to ToolUnavailable. It is a tool
            // availability contract error, not a bound-surface/executor
            // mismatch.
            (
                "Error: tool agent_fanout is not available in this turn",
                ErrorKind::ToolUnavailable,
            ),
            // Unknown
            (
                "something completely unexpected happened",
                ErrorKind::Unknown,
            ),
            (
                "Model selection is required. Select a concrete model with `/model set <name>`.",
                ErrorKind::MissingModelSelection,
            ),
            ("", ErrorKind::Unknown),
            ("   \n\t  ", ErrorKind::Unknown),
        ];
        for &(input, expected) in cases {
            assert_eq!(
                classify_tool_output(input),
                expected,
                "classify_tool_output({input:?}) should be {expected:?}"
            );
        }
    }

    #[test]
    fn classify_very_long_input() {
        let long = "x".repeat(100_000);
        assert_eq!(classify_tool_output(&long), ErrorKind::Unknown);
    }

    #[test]
    fn classify_priority_rules() {
        // Resource limit wins over network
        assert_eq!(
            classify_tool_output("fork: Resource temporarily unavailable (connection timeout)"),
            ErrorKind::ResourceLimit
        );
        // "command timed out" = ToolTimeout, not Network
        assert_eq!(
            classify_tool_output("command timed out after 30s"),
            ErrorKind::ToolTimeout
        );
        // "connection timed out" = Network
        assert_eq!(
            classify_tool_output("connection timed out after 30s"),
            ErrorKind::Network
        );
        // DB pool timeouts must not collapse into generic network timeouts.
        assert_eq!(
            classify_tool_output("DB query: connection pool timed out after 5s"),
            ErrorKind::DatabaseError
        );
    }

    #[test]
    fn classified_error_from_recovers_model_resolution_database_error() {
        let err = ClassifiedError::from(
            "Model resolution failed: DB query: error communicating with database: expected to read 4 bytes, got 0 bytes at EOF"
                .to_string(),
        );
        assert_eq!(err.kind, ErrorKind::DatabaseError);
        assert_eq!(
            err.message,
            "Model resolution failed: DB query: error communicating with database: expected to read 4 bytes, got 0 bytes at EOF"
        );
    }

    #[test]
    fn classify_oom_word_boundary() {
        // "oom" must match the OOM-killer pattern, NOT substrings inside
        // normal words like "room", "zoom", "bloom", "chatroom".
        assert_eq!(
            classify_tool_output("OOM killer invoked"),
            ErrorKind::ResourceLimit,
        );
        assert_eq!(
            classify_tool_output("oom: process killed"),
            ErrorKind::ResourceLimit,
        );
        // False positives that must NOT match ResourceLimit:
        for innocent in [
            "entering the chatroom now",
            "zoom meeting failed",
            "blooming flowers render",
            "room not found",
            "vroom vroom",
        ] {
            assert_ne!(
                classify_tool_output(innocent),
                ErrorKind::ResourceLimit,
                "{innocent:?} must not be ResourceLimit"
            );
        }
    }

    #[test]
    fn classify_http_status_codes_require_context() {
        // Bare "500" inside a line number or filename must not trigger Network.
        assert_eq!(
            classify_tool_output("Error at line 500 of parser.rs"),
            ErrorKind::Unknown,
            "line numbers must not match HTTP status codes"
        );
        // But actual HTTP errors should still match:
        assert_eq!(
            classify_tool_output("HTTP 500 Internal Server Error"),
            ErrorKind::Network,
        );
    }

    // ── New variant & diagnosis-hint checks ──

    #[test]
    fn new_error_variants() {
        // DatabaseError
        let k = ErrorKind::DatabaseError;
        assert_eq!(k.as_str(), "database_error");
        assert_eq!(ErrorKind::parse_tag("database_error"), Some(k));
        assert!(!k.is_retryable(), "DB errors must not auto-retry");
        assert!(k.retry_delay_ms(0).is_none());
        // Stall
        let s = ErrorKind::Stall;
        assert_eq!(s.as_str(), "stall");
        assert_eq!(ErrorKind::parse_tag("stall"), Some(s));
        assert!(!s.is_retryable(), "stall must not auto-retry");
        // classify_tool_output for SQL/DB errors
        for st in [
            "SQL syntax error: column must appear in GROUP BY",
            "error returned from database: deadlock found",
            "sqlx: connection pool timed out",
            "deadlock detected on table x",
            "matrixone pool timed out: query cancelled",
            "duplicate entry for key 'PRIMARY'",
            "foreign key constraint fails",
        ] {
            assert_eq!(classify_tool_output(st), ErrorKind::DatabaseError);
        }
    }

    #[test]
    fn diagnosis_hint_specific_keywords() {
        // Operator-specific diagnosis_hint content (vs. LLM-facing guidance)
        assert!(
            ErrorKind::ResourceLimit
                .diagnosis_hint()
                .to_lowercase()
                .contains("ulimit")
        );
        let auth = ErrorKind::Auth.diagnosis_hint().to_lowercase();
        assert!(auth.contains("login") || auth.contains("credential") || auth.contains("token"));
        let db = ErrorKind::DatabaseError.diagnosis_hint().to_lowercase();
        assert!(db.contains("matrixone") || db.contains("sql") || db.contains("connect"));
        let stall = ErrorKind::Stall.diagnosis_hint().to_lowercase();
        assert!(stall.contains("rewind") || stall.contains("model") || stall.contains("loop"));
        // Must differ from guidance for operator-relevant variants
        for kind in [
            ErrorKind::ResourceLimit,
            ErrorKind::Auth,
            ErrorKind::DatabaseError,
            ErrorKind::Stall,
        ] {
            assert_ne!(kind.diagnosis_hint(), kind.guidance());
        }
    }

    #[test]
    fn context_window_error_detected_in_llm_error_format() {
        let api_body = r#"{"error":{"message":"This model's maximum context length is 128000 tokens","type":"invalid_request_error"}}"#;
        let err = format!("LLM error 400: {api_body}");
        assert!(is_llm_context_window_error(&err));
    }
}
