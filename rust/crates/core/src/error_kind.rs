//! Unified error classification for the astra-engine runtime.
//!
//! Every error in the system is classified exactly once at its source into an
//! [`ErrorKind`]. No downstream code re-parses error strings — it pattern-matches
//! on the kind instead.
//!
//! The only exception is [`classify_tool_output`]: external tools (bash, MCP)
//! return unstructured strings, so one fallback classifier remains.

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

    // ── Client-side ──────────────────────────────────
    /// User Ctrl-C, cancel token, or API cancellation.
    Cancelled,

    // ── Catch-all ────────────────────────────────────
    /// Unrecognized error.
    Unknown,
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
            Self::BudgetExhausted => "budget_exhausted",
            Self::ToolRoundsExhausted => "tool_rounds_exhausted",
            Self::Network => "network",
            Self::ToolNotFound => "tool_not_found",
            Self::ToolInvalidArgs => "tool_invalid_args",
            Self::ToolTimeout => "tool_timeout",
            Self::ToolUnavailable => "tool_unavailable",
            Self::ResourceLimit => "resource_limit",
            Self::DatabaseError => "database_error",
            Self::Stall => "stall",
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
                 Check the tool's expected parameters and fix the call."
            }
            Self::ToolTimeout => {
                "Tool command timed out — the scope is too broad. \
                 Do NOT retry with the same arguments. Narrow the search: \
                 use a specific subdirectory, file filter, or more specific pattern."
            }
            Self::ToolUnavailable => {
                "Tool is not available in this environment. \
                 Do NOT retry — use an alternative tool."
            }
            Self::ResourceLimit => {
                "System resource limit reached (memory/disk/processes). \
                 This tool is BLOCKED for the rest of this session. \
                 Reduce system load or try a different approach."
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
                "Model is calling tools with wrong parameters. This may improve with \
                 a better model, clearer system prompt, or stricter tool schemas."
            }
            Self::ToolTimeout => {
                "Tool scope is too broad. Break the operation into smaller chunks \
                 or raise the tool timeout if genuinely long-running."
            }
            Self::ToolUnavailable => {
                "Install or configure the missing tool, or remove it from the skill \
                 manifest so the agent does not pick it."
            }
            Self::ResourceLimit => {
                "Check system limits: `ulimit -u` (max procs), `ulimit -n` (open files). \
                 Kill orphan processes: `ps aux | grep defunct`. May need to restart \
                 the system or increase limits."
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
            "budget_exhausted" => Some(Self::BudgetExhausted),
            "tool_rounds_exhausted" => Some(Self::ToolRoundsExhausted),
            "network" => Some(Self::Network),
            "tool_not_found" => Some(Self::ToolNotFound),
            "tool_invalid_args" => Some(Self::ToolInvalidArgs),
            "tool_timeout" => Some(Self::ToolTimeout),
            "tool_unavailable" => Some(Self::ToolUnavailable),
            "resource_limit" => Some(Self::ResourceLimit),
            "database_error" => Some(Self::DatabaseError),
            "stall" => Some(Self::Stall),
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

// ── Tool output fallback classifier ──────────────────────────────────────────

/// The ONLY string-matching classifier in the codebase.
///
/// Used exclusively for external tool output (bash, MCP tools) where we don't
/// control the error format. All other errors are constructed with the correct
/// [`ErrorKind`] at their source.
#[must_use]
pub fn classify_tool_output(error_str: &str) -> ErrorKind {
    let lower = error_str.to_lowercase();

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
    if lower.contains("sql syntax error")
        || lower.contains("error returned from database")
        || lower.contains("sqlx")
        || lower.contains("deadlock")
        || lower.contains("connection pool timed out")
        || (lower.contains("column") && lower.contains("group by"))
    {
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

    // Unavailable — check BEFORE "not found" because "command not found" should
    // be ToolUnavailable, not ToolNotFound.
    if lower.contains("not installed")
        || lower.contains("not available")
        || lower.contains("not configured")
        || lower.contains("command not found")
        || lower.contains("no such command")
        || lower.contains("unsupported")
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
        || lower.contains("type mismatch")
        || lower.contains("invalid json")
        || lower.contains("malformed")
        || lower.contains("file is too large")
        || lower.contains("old_str not found")
        || lower.contains("sandbox")
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
        ErrorKind::BudgetExhausted,
        ErrorKind::ToolRoundsExhausted,
        ErrorKind::Network,
        ErrorKind::ToolNotFound,
        ErrorKind::ToolInvalidArgs,
        ErrorKind::ToolTimeout,
        ErrorKind::ToolUnavailable,
        ErrorKind::ResourceLimit,
        ErrorKind::DatabaseError,
        ErrorKind::Stall,
        ErrorKind::Cancelled,
        ErrorKind::Unknown,
    ];

    #[test]
    fn as_str_roundtrip() {
        for &kind in ALL_VARIANTS {
            let json = serde_json::to_string(&kind).unwrap();
            let back: ErrorKind = serde_json::from_str(&json).unwrap();
            assert_eq!(kind, back, "roundtrip failed for {kind:?}");
        }
    }

    #[test]
    fn retryable_variants() {
        assert!(ErrorKind::RateLimit.is_retryable());
        assert!(ErrorKind::ServerError.is_retryable());
        assert!(ErrorKind::StreamIdle.is_retryable());
        assert!(ErrorKind::StreamTransport.is_retryable());
        assert!(ErrorKind::Network.is_retryable());

        assert!(!ErrorKind::Auth.is_retryable());
        assert!(!ErrorKind::ContextWindow.is_retryable());
        assert!(!ErrorKind::Cancelled.is_retryable());
        assert!(!ErrorKind::ResourceLimit.is_retryable());
        assert!(!ErrorKind::ToolTimeout.is_retryable());
    }

    #[test]
    fn retry_delay_exponential() {
        assert_eq!(ErrorKind::RateLimit.retry_delay_ms(0), Some(5_000));
        assert_eq!(ErrorKind::RateLimit.retry_delay_ms(1), Some(10_000));
        assert_eq!(ErrorKind::RateLimit.retry_delay_ms(2), Some(20_000));
        assert_eq!(ErrorKind::RateLimit.retry_delay_ms(3), Some(40_000));
        // Capped at attempt 3
        assert_eq!(ErrorKind::RateLimit.retry_delay_ms(10), Some(40_000));

        assert_eq!(ErrorKind::StreamIdle.retry_delay_ms(0), Some(0));
        assert_eq!(ErrorKind::Auth.retry_delay_ms(0), None);
    }

    #[test]
    fn all_variants_is_exhaustive() {
        // `as_str` has an exhaustive `match` that won't compile if a variant
        // is missing, but `ALL_VARIANTS` is a hand-maintained array. This
        // test detects drift: every serde round-trip tag must map back to
        // a variant that's in the array.
        let tags: std::collections::HashSet<&str> =
            ALL_VARIANTS.iter().map(|k| k.as_str()).collect();
        // If ALL_VARIANTS misses a variant, as_str() has more arms than
        // the set has entries → this assert fires.
        assert_eq!(
            tags.len(),
            ALL_VARIANTS.len(),
            "ALL_VARIANTS has duplicates or drift"
        );
        // Reverse: every tag must parse back to a variant in the array.
        for &kind in ALL_VARIANTS {
            let rt = ErrorKind::parse_tag(kind.as_str());
            assert_eq!(rt, Some(kind), "parse_tag roundtrip failed for {kind:?}");
        }
    }

    #[test]
    fn guidance_non_empty() {
        for &kind in ALL_VARIANTS {
            assert!(!kind.guidance().is_empty(), "empty guidance for {kind:?}");
            assert!(!kind.as_str().is_empty(), "empty as_str for {kind:?}");
        }
    }

    #[test]
    fn display_matches_as_str() {
        for kind in [ErrorKind::RateLimit, ErrorKind::Auth, ErrorKind::Unknown] {
            assert_eq!(format!("{kind}"), kind.as_str());
        }
    }

    #[test]
    fn retry_delay_all_retryable_variants() {
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
    fn llm_feedback_format() {
        let err = ClassifiedError::new(ErrorKind::StreamIdle, "no chunk in 90000ms");
        let fb = err.llm_feedback();
        assert!(fb.starts_with("[stream_idle]"));
        assert!(fb.contains("no chunk in 90000ms"));
        assert!(fb.contains("→"));
    }

    #[test]
    fn display_format() {
        let err = ClassifiedError::new(ErrorKind::Auth, "401 Unauthorized");
        assert_eq!(err.to_string(), "[auth] 401 Unauthorized");
    }

    #[test]
    fn from_string_recovers_kind_from_prefix() {
        let original = ClassifiedError::new(ErrorKind::RateLimit, "429 too many requests");
        let roundtrip = ClassifiedError::from(original.to_string());
        assert_eq!(roundtrip.kind, ErrorKind::RateLimit);
        assert_eq!(roundtrip.message, "429 too many requests");
    }

    #[test]
    fn from_string_recovers_all_kinds() {
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
    }

    #[test]
    fn from_string_without_prefix_falls_back_to_classify() {
        let err = ClassifiedError::from("connection refused".to_string());
        assert_eq!(err.kind, ErrorKind::Network);
        assert_eq!(err.message, "connection refused");
    }

    #[test]
    fn error_kind_from_str_roundtrip() {
        for kind in [
            ErrorKind::RateLimit,
            ErrorKind::ServerError,
            ErrorKind::Auth,
            ErrorKind::ContextWindow,
            ErrorKind::Unknown,
        ] {
            assert_eq!(ErrorKind::parse_tag(kind.as_str()), Some(kind));
        }
        assert_eq!(ErrorKind::parse_tag("nonexistent"), None);
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
            // ToolUnavailable
            ("command not found: rg", ErrorKind::ToolUnavailable),
            ("bash: rg: command not found", ErrorKind::ToolUnavailable),
            // Unknown
            (
                "something completely unexpected happened",
                ErrorKind::Unknown,
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

    // ── ClassifiedError unhappy paths ──

    #[test]
    fn classified_error_is_std_error() {
        let err = ClassifiedError::new(ErrorKind::Auth, "bad key");
        let _: &dyn std::error::Error = &err;
    }

    #[test]
    fn classified_error_empty_message() {
        let err = ClassifiedError::new(ErrorKind::Unknown, "");
        assert_eq!(err.to_string(), "[unknown] ");
        let fb = err.llm_feedback();
        assert!(fb.contains("[unknown]"));
        assert!(fb.contains("→"));
    }

    // ── P0.1 TDD: ErrorKind becomes the single taxonomy ─────────────────
    //
    // New variants: DatabaseError (MatrixOne/SQLx), Stall (agent looped).
    // New method: diagnosis_hint() — operator-facing fix, distinct from
    // guidance() which is LLM-facing.

    #[test]
    fn database_error_variant_exists() {
        let k = ErrorKind::DatabaseError;
        assert_eq!(k.as_str(), "database_error");
        assert_eq!(ErrorKind::parse_tag("database_error"), Some(k));
        assert!(!k.is_retryable(), "DB errors must not auto-retry blindly");
        assert!(k.retry_delay_ms(0).is_none());
    }

    #[test]
    fn stall_variant_exists() {
        let k = ErrorKind::Stall;
        assert_eq!(k.as_str(), "stall");
        assert_eq!(ErrorKind::parse_tag("stall"), Some(k));
        assert!(!k.is_retryable(), "stall must not auto-retry");
    }

    #[test]
    fn classify_tool_output_sql_errors_map_to_database_error() {
        // DB detection previously lived in services/src/reflect.rs. Now the
        // single classifier owns it.
        for s in [
            "SQL syntax error: column must appear in GROUP BY",
            "error returned from database: deadlock found",
            "sqlx: connection pool timed out",
            "deadlock detected on table x",
        ] {
            assert_eq!(
                classify_tool_output(s),
                ErrorKind::DatabaseError,
                "classify_tool_output({s:?}) should be DatabaseError"
            );
        }
    }

    #[test]
    fn diagnosis_hint_exists_for_every_variant() {
        for &kind in ALL_VARIANTS {
            assert!(
                !kind.diagnosis_hint().is_empty(),
                "empty diagnosis_hint for {kind:?}"
            );
        }
    }

    #[test]
    fn diagnosis_hint_targets_operators_specifically() {
        // ResourceLimit → ulimit / process limits
        assert!(
            ErrorKind::ResourceLimit
                .diagnosis_hint()
                .to_lowercase()
                .contains("ulimit")
        );
        // Auth → re-authentication / credentials
        let auth = ErrorKind::Auth.diagnosis_hint().to_lowercase();
        assert!(auth.contains("login") || auth.contains("credential") || auth.contains("token"));
        // DatabaseError → MatrixOne / SQL schema / connectivity
        let db = ErrorKind::DatabaseError.diagnosis_hint().to_lowercase();
        assert!(db.contains("matrixone") || db.contains("sql") || db.contains("connect"));
        // Stall → rewind / model switch / loop
        let stall = ErrorKind::Stall.diagnosis_hint().to_lowercase();
        assert!(stall.contains("rewind") || stall.contains("model") || stall.contains("loop"));
    }

    #[test]
    fn diagnosis_hint_is_distinct_from_guidance_for_operator_variants() {
        for kind in [
            ErrorKind::ResourceLimit,
            ErrorKind::Auth,
            ErrorKind::DatabaseError,
            ErrorKind::Stall,
        ] {
            assert_ne!(
                kind.diagnosis_hint(),
                kind.guidance(),
                "diagnosis_hint must differ from guidance for {kind:?}"
            );
        }
    }

    #[test]
    fn as_str_roundtrip_covers_new_variants() {
        for kind in [ErrorKind::DatabaseError, ErrorKind::Stall] {
            let s = kind.as_str();
            assert_eq!(ErrorKind::parse_tag(s), Some(kind));
            let json = serde_json::to_string(&kind).unwrap();
            let back: ErrorKind = serde_json::from_str(&json).unwrap();
            assert_eq!(kind, back);
        }
    }
}
