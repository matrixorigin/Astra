//! Unified error classification for the Astra runtime.
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
    /// 402, provider balance or billing limit prevents inference.
    PaymentRequired,
    /// An explicit provider machine code reports exhausted balance or quota.
    ProviderQuotaExhausted,
    /// Provider denied inference (403), without evidence of invalid credentials.
    ProviderForbidden,
    /// 5xx from provider.
    ServerError,
    /// 401 or an explicit invalid-credential code; never inferred from 403 alone.
    Auth,
    /// Prompt too long for the model's context window.
    ContextWindow,
    /// 400 other (duplicate function name, bad schema, malformed request).
    InvalidRequest,

    // ── Internal contracts ──────────────────────────────────
    /// A runtime producer/consumer protocol or lifecycle invariant was
    /// violated. This is an implementation defect, not invalid user input.
    ContractViolation,

    // ── Streaming ────────────────────────────────────
    /// No SSE chunk received within the idle timeout.
    StreamIdle,
    /// One physical provider inference exceeded its wall-clock or semantic
    /// progress deadline. This is not the agent turn/run budget.
    ProviderDeadline,
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
    /// A sandbox, safety guard, or execution policy intentionally denied the operation.
    PolicyDenied,
    /// Local command timed out (grep on huge repo, long-running bash).
    ToolTimeout,
    /// Tool not installed, not configured, or explicitly unavailable.
    ToolUnavailable,
    /// Runtime advertised a tool but failed to bind an executor/transport for it.
    ToolBinding,
    /// The tool may have applied an external effect, but its acknowledgement was lost.
    ToolOutcomeUnknown,
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
    /// Dispatch may have completed, so a new call requires outcome reconciliation first.
    OutcomeUnknown,
    ResourceExhausted,
    /// The tool invocation itself was valid, but an invoked command returned
    /// a non-semantic failure status. This is deliberately distinct from
    /// [`InvalidArguments`]: shell arguments may be valid while a path,
    /// command composition, or the command's own operation is wrong.
    CommandFailed,
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
    ReconcileOutcome,
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

    /// Build the default recovery contract for a classified failure at an
    /// unstructured boundary (for example legacy edge output).  Producers
    /// with richer domain context should construct [`Self::new`] directly;
    /// this helper exists so every fallback boundary uses the same typed
    /// cause/action mapping instead of inventing its own retry semantics.
    #[must_use]
    pub fn from_error_kind(kind: ErrorKind) -> Self {
        let (cause, action) = match kind {
            ErrorKind::ToolInvalidArgs | ErrorKind::InvalidRequest => (
                ToolFailureCause::InvalidArguments,
                ToolRecoveryAction::CorrectArguments,
            ),
            ErrorKind::ToolNotFound => (
                ToolFailureCause::ResourceMissing,
                ToolRecoveryAction::VerifyResource,
            ),
            ErrorKind::PolicyDenied | ErrorKind::Auth => (
                ToolFailureCause::PermissionBoundary,
                ToolRecoveryAction::SelectAvailableCapability,
            ),
            ErrorKind::ToolUnavailable | ErrorKind::ToolBinding => (
                ToolFailureCause::CapabilityUnavailable,
                ToolRecoveryAction::SelectAvailableCapability,
            ),
            ErrorKind::ToolOutcomeUnknown => (
                ToolFailureCause::OutcomeUnknown,
                ToolRecoveryAction::ReconcileOutcome,
            ),
            ErrorKind::ResourceLimit | ErrorKind::ConnectionPoolExhausted => (
                ToolFailureCause::ResourceExhausted,
                ToolRecoveryAction::ReduceResourcePressure,
            ),
            ErrorKind::ToolTimeout
            | ErrorKind::Network
            | ErrorKind::RateLimit
            | ErrorKind::ServerError
            | ErrorKind::StreamIdle
            | ErrorKind::StreamTransport => (
                ToolFailureCause::TransientTransport,
                ToolRecoveryAction::WaitAndRetry,
            ),
            _ => (
                ToolFailureCause::Unknown,
                ToolRecoveryAction::InspectStructuredFailure,
            ),
        };
        Self::new(kind, cause, kind.is_retryable(), vec![action])
    }
}

impl ErrorKind {
    /// Stable string tag for journal/serialization.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::RateLimit => "rate_limit",
            Self::PaymentRequired => "payment_required",
            Self::ProviderQuotaExhausted => "provider_quota_exhausted",
            Self::ProviderForbidden => "provider_forbidden",
            Self::ServerError => "server_error",
            Self::Auth => "auth",
            Self::ContextWindow => "context_window",
            Self::InvalidRequest => "invalid_request",
            Self::ContractViolation => "contract_violation",
            Self::StreamIdle => "stream_idle",
            Self::ProviderDeadline => "provider_deadline",
            Self::StreamTransport => "stream_transport",
            Self::ConnectionPoolExhausted => "connection_pool_exhausted",
            Self::BudgetExhausted => "budget_exhausted",
            Self::ToolRoundsExhausted => "tool_rounds_exhausted",
            Self::Network => "network",
            Self::ToolNotFound => "tool_not_found",
            Self::ToolInvalidArgs => "tool_invalid_args",
            Self::PolicyDenied => "policy_denied",
            Self::ToolTimeout => "tool_timeout",
            Self::ToolUnavailable => "tool_unavailable",
            Self::ToolBinding => "tool_binding",
            Self::ToolOutcomeUnknown => "tool_outcome_unknown",
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
            Self::ProviderQuotaExhausted => {
                "The model provider account has exhausted its balance or quota. \
                 Do NOT retry or re-authenticate; ask the provider account owner to resolve the restriction."
            }
            Self::ProviderForbidden => {
                "The model provider denied inference. The cause is not known. \
                 Do NOT retry automatically; ask the model administrator to check the provider rejection."
            }
            Self::PaymentRequired => {
                "Model inference requires available credit or a higher billing limit. \
                 Do NOT retry or re-authenticate; resolve the provider billing restriction first."
            }
            Self::RateLimit => {
                "Rate limit hit. Check the run's recovery status; reduce parallel requests if this persists."
            }
            Self::ServerError => {
                "LLM provider returned a server error (5xx). Check the run's recovery status before retrying."
            }
            Self::Auth => {
                "Authentication failed. \
                 Do NOT retry — credentials need to be refreshed."
            }
            Self::ContextWindow => {
                "Prompt exceeds the model's context window. \
                 Reduce input size: drop older messages, summarize, or use a model with a larger context."
            }
            Self::InvalidRequest => {
                "The request was rejected by the LLM provider. Do NOT retry with the same parameters; \
                 reduce request size if the provider reports a body-size limit."
            }
            Self::ContractViolation => {
                "The runtime violated an internal lifecycle or delivery contract. \
                 Stop this execution path and preserve the structured failure for operator diagnosis."
            }
            Self::StreamIdle => {
                "Model stopped sending tokens mid-stream (idle timeout). \
                 Check the run's recovery status; if this recurs, reduce the input size or ask the user about selecting another model."
            }
            Self::ProviderDeadline => {
                "The current provider inference exceeded its deadline. Preserve partial evidence \
                 and use at most one bounded convergence step when it is safe."
            }
            Self::StreamTransport => {
                "The model response stream was interrupted. Check the run's recovery status before retrying."
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
                 Check the run's recovery status and network connectivity if this persists."
            }
            Self::ToolNotFound => {
                "Tool or resource not found. \
                 Verify the name/path is correct before retrying."
            }
            Self::ToolInvalidArgs => {
                "Invalid arguments for the tool. \
                 Fix the parameters and retry the same tool before switching tools."
            }
            Self::PolicyDenied => {
                "The operation was denied by an execution safety policy. \
                 Do NOT retry the identical call. Remove the hazardous operation, use a permitted \
                 structured tool, or request approval only when the policy exposes that path."
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
            Self::ToolOutcomeUnknown => {
                "The tool may have applied its operation, but the acknowledgement was lost. \
                 Reconcile the provider's state before deciding whether any new call is safe."
            }
            Self::ResourceLimit => {
                "System resource limit reached (memory/disk/processes). \
                 This tool is BLOCKED for the rest of this session. \
                 Reduce system load or try a different approach."
            }
            Self::ConnectionPoolExhausted => {
                "HTTP connection pool saturated (reqwest/hyper). \
                 Reduce parallel LLM requests, check for response body leaks, \
                 or increase pool_max_size. Check the run's recovery status before retrying."
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
            Self::PaymentRequired => "Check the model provider balance and billing limits.",
            Self::ProviderQuotaExhausted => {
                "Ask the model provider account owner to check its balance and quota."
            }
            Self::ProviderForbidden => {
                "Ask the model administrator to inspect the provider rejection; a 403 alone does not identify its cause."
            }
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
            Self::ContractViolation => {
                "Inspect the structured source/error_code fields and repair the producer/consumer \
                 contract. Treat this as a runtime defect, not user input or a retryable outage."
            }
            Self::StreamIdle => {
                "Model stalled mid-stream. If recurring, switch model or reduce input size."
            }
            Self::ProviderDeadline => {
                "Inspect the typed deadline phase and elapsed time. This is a provider-attempt \
                 deadline, not evidence that the agent turn/session budget was exhausted."
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
            Self::PolicyDenied => {
                "Inspect the named sandbox or safety rule and the concrete operation it rejected. \
                 Keep approval policy centralized; do not duplicate blanket bans in executors."
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
            Self::ToolOutcomeUnknown => {
                "Inspect the provider's state or invocation receipt to determine whether the \
                 operation was applied. Do not replay an uncertain external effect."
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
            "payment_required" => Some(Self::PaymentRequired),
            "provider_quota_exhausted" => Some(Self::ProviderQuotaExhausted),
            "provider_forbidden" => Some(Self::ProviderForbidden),
            "server_error" => Some(Self::ServerError),
            "auth" => Some(Self::Auth),
            "context_window" => Some(Self::ContextWindow),
            "invalid_request" => Some(Self::InvalidRequest),
            "contract_violation" => Some(Self::ContractViolation),
            "stream_idle" => Some(Self::StreamIdle),
            "provider_deadline" => Some(Self::ProviderDeadline),
            "stream_transport" => Some(Self::StreamTransport),
            "connection_pool_exhausted" => Some(Self::ConnectionPoolExhausted),
            "budget_exhausted" => Some(Self::BudgetExhausted),
            "tool_rounds_exhausted" => Some(Self::ToolRoundsExhausted),
            "network" => Some(Self::Network),
            "tool_not_found" => Some(Self::ToolNotFound),
            "tool_invalid_args" => Some(Self::ToolInvalidArgs),
            "policy_denied" => Some(Self::PolicyDenied),
            "tool_timeout" => Some(Self::ToolTimeout),
            "tool_unavailable" => Some(Self::ToolUnavailable),
            "tool_binding" => Some(Self::ToolBinding),
            "tool_outcome_unknown" => Some(Self::ToolOutcomeUnknown),
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

// ── Structured LLM provider error envelope ───────────────────────────────────

/// One parsed provider error response, shared by all classifiers at an HTTP
/// boundary. Keeping the parsed root avoids reparsing and cloning the same
/// envelope for kind, machine facts, parameters, and correlation IDs.
///
/// Gateways such as TaaS return
/// `{"error":{"message","type","param","code","request_id"}}`; Anthropic
/// additionally places `request_id` at the top level. Explicit protocol codes
/// classify authoritatively, while the generic Anthropic error family requires
/// a specific semantic overflow cause.
#[derive(Debug)]
pub struct StructuredLlmError {
    value: serde_json::Value,
}

impl StructuredLlmError {
    /// Parse a raw provider body or a message with JSON appended after a
    /// human-readable prefix. A JSON response without an `error` member is not
    /// a provider error projection.
    #[must_use]
    pub fn parse(body: &str) -> Option<Self> {
        let trimmed = body.trim();
        let value = serde_json::from_str::<serde_json::Value>(trimmed)
            .ok()
            .or_else(|| {
                let start = trimmed.find('{')?;
                serde_json::from_str::<serde_json::Value>(trimmed.get(start..)?).ok()
            })?;
        value.get("error")?;
        Some(Self { value })
    }

    fn error(&self) -> &serde_json::Value {
        // `parse` establishes this invariant.
        self.value.get("error").expect("parsed provider error")
    }

    /// Borrow a string field from the provider's `error` object.
    #[must_use]
    pub fn error_field(&self, key: &str) -> Option<&str> {
        self.error().get(key)?.as_str()
    }

    /// The whitelisted machine-readable cause and its recovery kind. A
    /// specific `type` cause outranks a generic `code` category.
    #[must_use]
    pub fn code(&self) -> Option<(ErrorKind, &'static str)> {
        let token = |key: &str| {
            self.error_field(key)
                .filter(|token| !token.trim().is_empty())
        };
        let code = token("code").and_then(llm_error_envelope_code_class);
        let category = token("type").and_then(llm_error_envelope_code_class);
        code.filter(|(_, token)| *token != "invalid_request")
            .or(category)
            .or(code)
    }

    /// Whether the normalized protocol cause is more specific than the
    /// generic `invalid_request` family.
    #[must_use]
    pub fn has_specific_cause(&self) -> bool {
        self.code()
            .is_some_and(|(_, token)| token != "invalid_request")
    }

    /// Derive recovery once for HTTP and untyped SSE consumers. Specific
    /// protocol causes (including rate limits) win. Generic or empty
    /// categories and numeric HTTP codes do not contradict explicit
    /// input-overflow semantics; specific string codes and non-input
    /// parameters still bound recovery.
    #[must_use]
    pub fn kind(&self) -> Option<ErrorKind> {
        let cause = self.code();
        if let Some((kind, token)) = cause
            && token != "invalid_request"
        {
            return Some(kind);
        }
        let specific_code = self
            .error_field("code")
            .filter(|token| !token.trim().is_empty())
            .is_some_and(|token| token != "invalid_request");
        let input_parameter = self.error_field("param").is_none_or(|parameter| {
            matches!(parameter, "input" | "input_tokens" | "messages" | "prompt")
        });
        if !specific_code
            && input_parameter
            && self.error_field("message").is_some_and(|message| {
                is_explicit_input_context_window_message_lower(&message.to_ascii_lowercase())
            })
        {
            Some(ErrorKind::ContextWindow)
        } else {
            cause.map(|(kind, _)| kind)
        }
    }

    /// Extract and validate either the OpenAI-compatible nested request ID or
    /// Anthropic's documented top-level `request_id`.
    #[must_use]
    pub fn request_id(&self) -> Option<String> {
        self.error_field("request_id")
            .and_then(validated_llm_provider_request_id)
            .or_else(|| {
                self.value
                    .get("request_id")?
                    .as_str()
                    .and_then(validated_llm_provider_request_id)
            })
    }

    /// Whether machine-readable cause fields are present. A message-only
    /// error object remains prose in JSON and does not count.
    #[must_use]
    pub fn has_facts(&self) -> bool {
        ["code", "type", "param"].iter().any(|key| {
            self.error_field(key)
                .is_some_and(|token| !token.trim().is_empty())
        })
    }
}

/// Classify a single provider error-envelope code token.
///
/// This is the same whitelist [`StructuredLlmError::code`] applies to
/// envelope bodies, exposed for boundaries that already hold the bare token
/// (e.g. the negotiated MOI model gateway contract). Returns the classified
/// kind alongside the canonical static token; unrecognized codes return
/// `None` so callers keep their existing fallback.
#[must_use]
pub fn llm_error_envelope_code_class(code: &str) -> Option<(ErrorKind, &'static str)> {
    // Responses already owns protocol alias normalization; every envelope
    // consumer uses that canonical cause instead of maintaining another list.
    let code = crate::model_wire::responses::provider_error(code)
        .map(|(normalized, _)| normalized)
        .unwrap_or(code);
    match code {
        "context_length_exceeded" => Some((ErrorKind::ContextWindow, "context_length_exceeded")),
        "request_body_too_large" => Some((ErrorKind::InvalidRequest, "request_body_too_large")),
        "invalid_request" => Some((ErrorKind::InvalidRequest, "invalid_request")),
        "provider_schema_error" => Some((ErrorKind::InvalidRequest, "provider_schema_error")),
        "rate_limit_error" => Some((ErrorKind::RateLimit, "rate_limit_error")),
        _ => None,
    }
}

/// Validate a provider-supplied request id before it crosses into
/// user-visible errors or structured log fields.
///
/// Provider error envelopes are untrusted: an arbitrary `request_id` could
/// carry echoed credentials, control characters, or unbounded diagnostics.
/// A character whitelist cannot tell an identifier from a credential —
/// `sk-proj-secretABC123` is alphanumerics and dashes — so only documented
/// identifier shapes pass:
///
/// - `req_<uuid>` — TaaS and current OpenAI request ids.
/// - `req_<hex>` — legacy OpenAI request ids (8..=96 hex digits).
/// - `req_<24 ASCII alphanumerics>` — Anthropic request ids.
/// - `<uuid>` — bare UUID request ids used by some gateways.
///
/// Credential shapes (`sk-…`, `Bearer …`, `AKIA…`), free text, and overlong
/// values are structurally excluded: they match none of these forms.
#[must_use]
pub fn validated_llm_provider_request_id(value: &str) -> Option<String> {
    const MAX_REQUEST_ID_LEN: usize = 128;
    const MAX_REQ_HEX_LEN: usize = 96;
    let value = value.trim();
    if value.is_empty() || value.len() > MAX_REQUEST_ID_LEN {
        return None;
    }
    let is_uuid = |token: &str| {
        token.len() == 36
            && token.bytes().enumerate().all(|(index, byte)| {
                if matches!(index, 8 | 13 | 18 | 23) {
                    byte == b'-'
                } else {
                    byte.is_ascii_hexdigit()
                }
            })
    };
    let valid = if let Some(token) = value.strip_prefix("req_") {
        is_uuid(token)
            || ((8..=MAX_REQ_HEX_LEN).contains(&token.len())
                && token.bytes().all(|byte| byte.is_ascii_hexdigit()))
            || (token.len() == 24 && token.bytes().all(|byte| byte.is_ascii_alphanumeric()))
    } else {
        is_uuid(value)
    };
    valid.then(|| value.to_string())
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
    is_explicit_input_context_window_message_lower(lower)
        || lower.contains("too many tokens")
        || is_max_tokens_context_limit_error(lower)
}

/// Provider-neutral input-overflow semantics shared by generic structured
/// errors. Output-token parameter matching deliberately stays only in the
/// unstructured fallback above, never in this generic-family recovery rule.
fn is_explicit_input_context_window_message_lower(lower: &str) -> bool {
    lower.contains("context_length_exceeded")
        || lower.contains("maximum context length")
        || lower.contains("prompt is too long")
        || lower.contains("input is too long")
        || lower.contains("context window")
        || (lower.contains("too many tokens")
            && (lower.contains("input") || lower.contains("prompt") || lower.contains("context")))
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
    if lower.contains("[genesis_access_denied]") {
        return ErrorKind::PolicyDenied;
    }
    // These are server-authored dependency errors, not an invalid selection or
    // a missing user credential. Preserve the stable code across the legacy
    // string-based resolution boundary rather than matching human prose.
    if [
        "[genesis_timeout]",
        "[genesis_not_ready]",
        "[genesis_credential_rejected]",
        "[moi_model_policy_unavailable]",
        "[uc_unavailable]",
        "[model_catalog_unavailable]",
        "[model_selection_changed]",
        "[model_admission_incomplete]",
    ]
    .iter()
    .any(|code| lower.contains(code))
    {
        return ErrorKind::ServerError;
    }
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
        || lower.contains("[model_selection_invalid]")
        || lower.contains("[model_admission_batch_invalid]")
        || lower.contains("[model_admission_batch_too_large]")
        || lower.contains("[model_offering_not_found]")
        || lower.contains("[model_offering_unavailable]")
        || lower.contains("[model_reasoning_unsupported]")
        || lower.contains("[model_purpose_unsupported]")
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
/// PolicyDenied > ResourceLimit > DatabaseError > ToolTimeout > Network > Auth >
/// ToolInvalidArgs(call contract) > ToolUnavailable >
/// ToolInvalidArgs(generic) > ToolNotFound > Unknown.
#[must_use]
pub fn classify_tool_output(error_str: &str) -> ErrorKind {
    let lower = error_str.to_lowercase();

    // Control tools carry a machine-owned lifecycle status. Preserve its
    // meaning before applying text heuristics: a registry timeout means the
    // operation's terminal state is unknown, not that the caller supplied bad
    // arguments. This applies to every producer using the shared status wire.
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(error_str)
        && let Some(status) = value.get("status").and_then(serde_json::Value::as_str)
    {
        match status {
            "registry_timeout" | "stop_status_unknown" => return ErrorKind::ToolTimeout,
            _ => {}
        }
    }

    // A structured memory conflict is an intentional write guard, not a
    // transport failure. Its repair is semantic (select/update the existing
    // memory, or explicitly opt out when distinct), so retrying the same call
    // can only repeat the conflict. Match this before generic retry words such
    // as `network`/`timeout`: recovery layers may append those words to an
    // already-produced tool result, and must not turn a deterministic conflict
    // into a retryable outage by doing so.
    if lower.contains("\"status\":\"conflict\"") && lower.contains("\"action_required\":\"update\"")
    {
        return ErrorKind::ToolInvalidArgs;
    }

    if lower.contains("model selection is required")
        || lower.contains("missing model selection")
        || lower.contains("no concrete model was selected")
    {
        return ErrorKind::MissingModelSelection;
    }

    // Explicit execution-policy decisions are neither authentication failures
    // nor malformed arguments. Preserve that distinction so recovery does not
    // ask the model to debug the provider/runtime/workspace or retry blindly.
    if lower.contains("sandbox_denied:")
        || lower.contains("policy denied:")
        || lower.contains("blocked by safety guard")
        || lower.contains("blocked by hook")
        || lower.contains("blocked by the sandbox")
        || lower.contains("blocked by local policy")
        || lower.contains("blocked while plan mode is active")
        || lower.contains("bash command blocked")
        || lower.contains("bash command matches a blocked destructive pattern")
        || lower.contains("piping download output into a shell is blocked")
        || lower.contains("networking in bash is blocked")
        || (lower.contains("sandbox:") && lower.contains(" is blocked"))
        || (lower.contains("recursive force deletion")
            && lower.contains("requires explicit confirmation"))
    {
        return ErrorKind::PolicyDenied;
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
        ErrorKind::PaymentRequired,
        ErrorKind::ProviderQuotaExhausted,
        ErrorKind::ProviderForbidden,
        ErrorKind::ServerError,
        ErrorKind::Auth,
        ErrorKind::ContextWindow,
        ErrorKind::InvalidRequest,
        ErrorKind::ContractViolation,
        ErrorKind::StreamIdle,
        ErrorKind::ProviderDeadline,
        ErrorKind::StreamTransport,
        ErrorKind::ConnectionPoolExhausted,
        ErrorKind::BudgetExhausted,
        ErrorKind::ToolRoundsExhausted,
        ErrorKind::Network,
        ErrorKind::ToolNotFound,
        ErrorKind::ToolInvalidArgs,
        ErrorKind::PolicyDenied,
        ErrorKind::ToolTimeout,
        ErrorKind::ToolUnavailable,
        ErrorKind::ToolBinding,
        ErrorKind::ToolOutcomeUnknown,
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
            ErrorKind::ConnectionPoolExhausted,
            ErrorKind::Network,
        ] {
            assert!(kind.is_retryable(), "{kind:?} must be retryable");
            let guidance = kind.guidance().to_ascii_lowercase();
            assert!(
                !guidance.contains("retrying automatically"),
                "{kind:?}: {guidance}"
            );
            assert!(!guidance.contains("will retry"), "{kind:?}: {guidance}");
        }
        // Known non-retryable
        for kind in [
            ErrorKind::Auth,
            ErrorKind::ContextWindow,
            ErrorKind::Cancelled,
            ErrorKind::ResourceLimit,
            ErrorKind::ToolTimeout,
            ErrorKind::ToolBinding,
            ErrorKind::PolicyDenied,
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
            ErrorKind::ProviderDeadline,
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

    fn projected_kind(body: &str) -> Option<ErrorKind> {
        StructuredLlmError::parse(body).and_then(|error| error.kind())
    }

    #[test]
    fn structured_kind_cases() {
        let cases: &[(&str, Option<ErrorKind>)] = &[
            // A 413 gateway byte limit is never model context overflow.
            (
                r#"{"error":{"message":"request body too large","type":"request_body_too_large","param":null,"code":"request_body_too_large","request_id":"req_abc123"}}"#,
                Some(ErrorKind::InvalidRequest),
            ),
            // An explicit envelope code takes precedence over message semantics.
            (
                r#"{"error":{"message":"This model's maximum context length is 128000 tokens.","type":"context_length_exceeded","param":null,"code":"context_length_exceeded","request_id":"req_def456"}}"#,
                Some(ErrorKind::ContextWindow),
            ),
            (
                r#"{"error":{"message":"unknown parameter 'temperature'","type":"invalid_request","param":"temperature","code":"invalid_request","request_id":"req_ghi789"}}"#,
                Some(ErrorKind::InvalidRequest),
            ),
            (
                r#"{"error":{"message":"schema rejected by provider","type":"provider_schema_error","param":"tools","code":"provider_schema_error","request_id":"req_jkl012"}}"#,
                Some(ErrorKind::InvalidRequest),
            ),
            // error.type is accepted when error.code is absent.
            (
                r#"{"error":{"message":"context exceeded","type":"context_length_exceeded"}}"#,
                Some(ErrorKind::ContextWindow),
            ),
            // Embedded in an "LLM error {status}: {json}" string.
            (
                r#"LLM error 413: {"error":{"message":"request body too large","type":"request_body_too_large","param":null,"code":"request_body_too_large","request_id":"req_mno345"}}"#,
                Some(ErrorKind::InvalidRequest),
            ),
            (
                r#"LLM error 400: {"error":{"message":"maximum context length exceeded","type":"context_length_exceeded","code":"context_length_exceeded","request_id":"req_pqr678"}}"#,
                Some(ErrorKind::ContextWindow),
            ),
            // A present but unrecognized `code` must not block the recognized
            // `type`: parameter/output-limit rejections stay InvalidRequest
            // instead of dropping to context-window text needles.
            (
                r#"{"error":{"message":"max_tokens is too large; maximum output is 8192","type":"invalid_request","param":"max_tokens","code":"unsupported_parameter"}}"#,
                Some(ErrorKind::InvalidRequest),
            ),
            (
                r#"{"error":{"message":"max_tokens is too large; maximum output is 8192","type":"invalid_request","param":"max_tokens","code":"invalid_value"}}"#,
                Some(ErrorKind::InvalidRequest),
            ),
            // Both cause tokens unrecognized → still None: no alias
            // whitelist extension. Presence of machine facts
            // ([`StructuredLlmError::has_facts`]), not this classifier, is
            // what keeps the prose fallback from reclaiming the envelope.
            (
                r#"{"error":{"message":"max_tokens is too large; maximum output is 8192","type":"invalid_request_error","param":"max_tokens","code":"invalid_value"}}"#,
                None,
            ),
            // No recognized code → caller falls back to needle classification.
            (
                r#"{"error":{"message":"rate limited","type":"rate_limit","code":"rate_limit_exceeded"}}"#,
                None,
            ),
            (r#"{"error":{"message":"no code here"}}"#, None),
            (r#"{"usage":{"tokens":1}}"#, None),
            // Unstructured text and invalid JSON → fallback.
            ("maximum context length exceeded", None),
            ("LLM error 413: not json at all", None),
            ("", None),
        ];

        for (input, expected) in cases {
            assert_eq!(
                projected_kind(input),
                *expected,
                "projected_kind({input:?}) should be {expected:?}"
            );
        }
    }

    #[test]
    fn structured_context_overflow_requires_specific_semantic_cause() {
        let anthropic = StructuredLlmError::parse(
            r#"{"type":"error","error":{"type":"invalid_request_error","message":"prompt is too long: 200000 tokens > 200000 maximum"},"request_id":"req_011CSHoEeqs5C35K2UUqR7Fy"}"#,
        )
        .expect("Anthropic envelope");
        assert_eq!(anthropic.kind(), Some(ErrorKind::ContextWindow));

        let openai = StructuredLlmError::parse(
            r#"{"error":{"code":null,"type":"invalid_request_error","param":null,"message":"This model's maximum context length is 8001 tokens, however you requested 8184 tokens (7672 in your prompt; 512 for the completion)"}}"#,
        )
        .expect("OpenAI envelope");
        assert_eq!(openai.kind(), Some(ErrorKind::ContextWindow));

        let numeric_input_code = StructuredLlmError::parse(
            r#"{"error":{"type":"BadRequestError","param":"input_tokens","code":400,"message":"This model's maximum context length is 3 tokens. However, your request has 8 input tokens. Please reduce the length of the input messages. (parameter=input_tokens, value=8)"}}"#,
        )
        .expect("numeric-code input-overflow envelope");
        assert_eq!(numeric_input_code.kind(), Some(ErrorKind::ContextWindow));

        for (body, normalized_cause) in [
            (
                r#"{"error":{"code":"context_window_exceeded","type":"invalid_request_error","message":"request rejected"}}"#,
                Some("context_length_exceeded"),
            ),
            (
                r#"{"error":{"code":"input_too_long","type":"invalid_request_error","message":"request rejected"}}"#,
                Some("context_length_exceeded"),
            ),
            (
                r#"{"error":{"code":null,"type":"invalid_request","message":"prompt is too long"}}"#,
                None,
            ),
            (
                r#"{"error":{"code":"","type":"invalid_request_error","message":"prompt is too long"}}"#,
                None,
            ),
        ] {
            let error = StructuredLlmError::parse(body).expect("structured envelope");
            assert_eq!(error.kind(), Some(ErrorKind::ContextWindow), "{body}");
            if let Some(normalized_cause) = normalized_cause {
                assert_eq!(error.code().map(|(_, code)| code), Some(normalized_cause));
            }
        }

        for body in [
            r#"{"error":{"type":"BadRequestError","param":"max_tokens","code":400,"message":"max_tokens is too large; maximum output is 8192"}}"#,
            r#"{"error":{"message":"max_tokens is too large; maximum output is 8192","type":"invalid_request_error","param":"max_tokens","code":"invalid_value"}}"#,
            r#"{"error":{"message":"max_tokens is too large; maximum output is 8192","type":"invalid_request_error","param":null,"code":null}}"#,
            r#"{"error":{"message":"This model's maximum context length is 128000 tokens.","type":"some_future_type","code":"some_future_code"}}"#,
            r#"{"error":{"message":"prompt is too long","type":"invalid_request_error","param":"max_tokens"}}"#,
        ] {
            let error = StructuredLlmError::parse(body).expect("structured envelope");
            assert_ne!(
                error.kind(),
                Some(ErrorKind::ContextWindow),
                "specific or unrecognized machine facts must not authorize compaction: {body}"
            );
        }
    }

    #[test]
    fn structured_rate_limit_cause_precedes_context_prose() {
        for message in [
            "This request would exceed the rate limit of 50,000 input tokens per minute",
            "Rate limit: too many tokens in the input per minute",
        ] {
            let body = serde_json::json!({"error":{"type":"rate_limit_error","message":message}});
            let error = StructuredLlmError::parse(&body.to_string()).expect("rate-limit envelope");
            assert_eq!(error.kind(), Some(ErrorKind::RateLimit), "{message}");
            assert_eq!(error.code().map(|(_, code)| code), Some("rate_limit_error"));
        }
    }

    fn projected_request_id(body: &str) -> Option<String> {
        StructuredLlmError::parse(body).and_then(|error| error.request_id())
    }

    #[test]
    fn structured_request_id_extraction() {
        assert_eq!(
            projected_request_id(
                r#"{"error":{"message":"request body too large","type":"request_body_too_large","param":null,"code":"request_body_too_large","request_id":"req_abc123de"}}"#
            ),
            Some("req_abc123de".to_string())
        );
        assert_eq!(
            projected_request_id(
                r#"LLM error 400: {"error":{"message":"bad param","code":"invalid_request","request_id":"req_def45678"}}"#
            ),
            Some("req_def45678".to_string())
        );
        assert_eq!(
            projected_request_id(r#"{"error":{"message":"bad param","code":"invalid_request"}}"#),
            None
        );
        assert_eq!(projected_request_id("not json"), None);
        assert_eq!(
            projected_request_id(
                r#"{"type":"error","error":{"type":"invalid_request_error","message":"prompt is too long"},"request_id":"req_011CSHoEeqs5C35K2UUqR7Fy"}"#
            ),
            Some("req_011CSHoEeqs5C35K2UUqR7Fy".to_string()),
            "Anthropic's documented top-level, non-hexadecimal id must correlate"
        );
    }

    #[test]
    fn structured_request_id_rejects_untrusted_values() {
        // Documented identifier shapes pass: TaaS's `req_<hyphenated-uuid>`,
        // legacy `req_<hex>`, and bare UUID gateway ids.
        assert_eq!(
            projected_request_id(
                r#"{"error":{"code":"invalid_request","request_id":"req_3fa85f64-5717-4562-b3fc-2c963f66afa6"}}"#
            ),
            Some("req_3fa85f64-5717-4562-b3fc-2c963f66afa6".to_string())
        );
        assert_eq!(
            projected_request_id(
                r#"{"error":{"code":"invalid_request","request_id":"3fa85f64-5717-4562-b3fc-2c963f66afa6"}}"#
            ),
            Some("3fa85f64-5717-4562-b3fc-2c963f66afa6".to_string())
        );
        // Secret-bearing values must never reach user-visible errors or logs.
        // A character whitelist cannot tell these credential shapes from an
        // identifier — only the structural forms above are accepted.
        for body in [
            r#"{"error":{"code":"invalid_request","request_id":"Bearer secret-token"}}"#,
            r#"{"error":{"code":"invalid_request","request_id":"sk-proj-secretABC123"}}"#,
            r#"{"error":{"code":"invalid_request","request_id":"sk-ant-api03-secretABC123"}}"#,
            r#"{"error":{"code":"invalid_request","request_id":"AKIAIOSFODNN7EXAMPLE"}}"#,
            // A `req_` prefix does not rescue a credential-shaped payload.
            r#"{"error":{"code":"invalid_request","request_id":"req_sk-proj-secretABC123"}}"#,
            // Control characters and quotes break log/message structure.
            r#"{"error":{"code":"invalid_request","request_id":"req_ok\u000aevil"}}"#,
            r#"{"error":{"code":"invalid_request","request_id":"req_\"quoted\""}}"#,
            r#"{"error":{"code":"invalid_request","request_id":"req spaced"}}"#,
        ] {
            assert_eq!(
                projected_request_id(body),
                None,
                "untrusted request_id must be rejected: {body}"
            );
        }
        // Overlong values are unbounded diagnostics, not identifiers.
        let overlong = format!(
            r#"{{"error":{{"code":"invalid_request","request_id":"req_{}"}}}}"#,
            "a".repeat(256)
        );
        assert_eq!(projected_request_id(&overlong), None);
    }

    fn projected_has_facts(body: &str) -> bool {
        StructuredLlmError::parse(body).is_some_and(|error| error.has_facts())
    }

    #[test]
    fn structured_facts_detect_machine_tokens() {
        // Any machine-readable cause token (code, type, or param) marks a
        // parsed provider error — even when every token is unrecognized and
        // the classification whitelist yields None.
        for body in [
            r#"{"error":{"code":"invalid_value","type":"invalid_request_error","param":"max_tokens","message":"max_tokens is too large; maximum output is 8192"}}"#,
            r#"{"error":{"code":"some_future_code","type":"some_future_type","message":"x"}}"#,
            r#"{"error":{"param":"max_tokens","message":"x"}}"#,
            r#"LLM error 400: {"error":{"type":"invalid_request_error","message":"x"}}"#,
        ] {
            assert!(projected_has_facts(body), "{body}");
        }
        // A message-only error object is prose in JSON; absent envelopes,
        // unusable token values, and plain text leave the needle fallback in
        // charge.
        for body in [
            r#"{"error":{"message":"maximum context length exceeded"}}"#,
            r#"{"error":{"code":null,"type":"","param":null,"message":"x"}}"#,
            r#"{"usage":{"tokens":1}}"#,
            "maximum context length exceeded",
            "",
        ] {
            assert!(!projected_has_facts(body), "{body}");
        }
    }

    fn projected_code(body: &str) -> Option<(ErrorKind, &'static str)> {
        StructuredLlmError::parse(body).and_then(|error| error.code())
    }

    #[test]
    fn structured_code_is_whitelisted() {
        // Only fixed protocol tokens are surfaced for user-visible messages;
        // unrecognized codes yield None so free-text diagnostics never leak.
        let cases: &[(&str, Option<&str>)] = &[
            (
                r#"{"error":{"code":"request_body_too_large","message":"request body too large"}}"#,
                Some("request_body_too_large"),
            ),
            (
                r#"{"error":{"code":"invalid_request","message":"bad param"}}"#,
                Some("invalid_request"),
            ),
            (
                r#"{"error":{"code":"provider_schema_error","message":"bad shape"}}"#,
                Some("provider_schema_error"),
            ),
            (
                r#"{"error":{"code":"context_length_exceeded","message":"too long"}}"#,
                Some("context_length_exceeded"),
            ),
            // type fallback carries the same whitelist.
            (
                r#"{"error":{"type":"request_body_too_large","message":"too large"}}"#,
                Some("request_body_too_large"),
            ),
            // OpenAI-compatible envelopes commonly carry `"code": null`;
            // unusable code values fall back to `type`.
            (
                r#"{"error":{"code":null,"type":"request_body_too_large","message":"too large"}}"#,
                Some("request_body_too_large"),
            ),
            (
                r#"{"error":{"code":"","type":"context_length_exceeded","message":"too long"}}"#,
                Some("context_length_exceeded"),
            ),
            (
                r#"{"error":{"code":123,"type":"invalid_request","message":"bad param"}}"#,
                Some("invalid_request"),
            ),
            // A usable `code` keeps precedence over `type`.
            (
                r#"{"error":{"code":"context_length_exceeded","type":"invalid_request","message":"too long"}}"#,
                Some("context_length_exceeded"),
            ),
            // An unrecognized `code` defers to a recognized `type`; the
            // surfaced token is the one that classified.
            (
                r#"{"error":{"code":"unsupported_parameter","type":"invalid_request","message":"max_tokens is too large; maximum output is 8192"}}"#,
                Some("invalid_request"),
            ),
            // Both tokens unrecognized → None, preserving the caller
            // text fallback.
            (
                r#"{"error":{"code":"unsupported_parameter","type":"some_new_type","message":"x"}}"#,
                None,
            ),
            (
                r#"{"error":{"code":"invalid_value","type":"invalid_request_error","message":"max_tokens is too large; maximum output is 8192"}}"#,
                None,
            ),
            (
                r#"{"error":{"code":"internal_error: db conn 10.0.0.1 refused","message":"x"}}"#,
                None,
            ),
        ];
        for (input, expected) in cases {
            assert_eq!(
                projected_code(input).map(|(_, code)| code),
                *expected,
                "projected_code({input:?}) should be {expected:?}"
            );
        }
    }

    #[test]
    fn classify_model_resolution_error_message_cases() {
        let cases: &[(&str, ErrorKind)] = &[
            (
                "[genesis_access_denied] Access denied.",
                ErrorKind::PolicyDenied,
            ),
            (
                "[genesis_credential_rejected] Authorization unavailable.",
                ErrorKind::ServerError,
            ),
            (
                "Model resolution failed: [genesis_timeout] Model access took too long.",
                ErrorKind::ServerError,
            ),
            (
                "Model resolution failed: [genesis_not_ready] Model access is unavailable.",
                ErrorKind::ServerError,
            ),
            (
                "Model resolution failed: [moi_model_policy_unavailable] Policy unavailable.",
                ErrorKind::ServerError,
            ),
            (
                "[model_catalog_unavailable] The authorized model catalog is temporarily unavailable.",
                ErrorKind::ServerError,
            ),
            (
                "[model_selection_changed] The selected model changed during admission; retry delegation.",
                ErrorKind::ServerError,
            ),
            (
                "[model_offering_unavailable] The requested model is unavailable.",
                ErrorKind::InvalidRequest,
            ),
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
            // PolicyDenied
            (
                "Error: bash command blocked (sensitive path access (/etc/))",
                ErrorKind::PolicyDenied,
            ),
            (
                "Error: bash command blocked (inline interpreter execution (python3 -c))\n⚠ bash failed with an unclassified tool error.",
                ErrorKind::PolicyDenied,
            ),
            (
                "Error: blocked by safety guard 'shell_obfuscation': command substitution",
                ErrorKind::PolicyDenied,
            ),
            (
                "SANDBOX_DENIED: Sandbox: path is outside the workspace",
                ErrorKind::PolicyDenied,
            ),
            (
                "Error: tool 'bash' is not available for this run binding: policy denied: tool is not allowed",
                ErrorKind::PolicyDenied,
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
            (
                r#"{"status":"conflict","action_required":"update","retry_hint":"retry remember only when distinct"}"#,
                ErrorKind::ToolInvalidArgs,
            ),
            (
                r#"{"status":"conflict","action_required":"update"}\n⚠ memory failed with a transient error (network/timeout)."#,
                ErrorKind::ToolInvalidArgs,
            ),
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
    fn fallback_failure_evidence_has_one_stable_mapping_per_error_kind() {
        let unavailable = ToolFailureEvidence::from_error_kind(ErrorKind::ToolUnavailable);
        assert_eq!(unavailable.cause, ToolFailureCause::CapabilityUnavailable);
        assert_eq!(
            unavailable.recovery_actions,
            vec![ToolRecoveryAction::SelectAvailableCapability]
        );
        assert!(!unavailable.retryable);

        let network = ToolFailureEvidence::from_error_kind(ErrorKind::Network);
        assert_eq!(network.cause, ToolFailureCause::TransientTransport);
        assert_eq!(
            network.recovery_actions,
            vec![ToolRecoveryAction::WaitAndRetry]
        );
        assert!(network.retryable);

        let timeout = ToolFailureEvidence::from_error_kind(ErrorKind::ToolTimeout);
        assert_eq!(timeout.cause, ToolFailureCause::TransientTransport);
        assert!(
            !timeout.retryable,
            "local command timeouts are not automatic retries"
        );

        let uncertain = ToolFailureEvidence::from_error_kind(ErrorKind::ToolOutcomeUnknown);
        assert_eq!(uncertain.cause, ToolFailureCause::OutcomeUnknown);
        assert_eq!(
            uncertain.recovery_actions,
            vec![ToolRecoveryAction::ReconcileOutcome]
        );
        assert!(!uncertain.retryable);
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

    #[test]
    fn control_timeout_status_is_not_misclassified_as_invalid_arguments() {
        for status in ["registry_timeout", "stop_status_unknown"] {
            let output = format!(
                r#"{{"ok":false,"kind":"background_task","status":"{status}","error":"registry did not respond"}}"#
            );
            assert_eq!(classify_tool_output(&output), ErrorKind::ToolTimeout);
        }
    }
}
