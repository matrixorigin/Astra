//! Error classification, retry policy, and alternative tool suggestion.
//!
//! Provides a systematic approach to non-happy-path handling:
//! 1. **Error classification** — delegates to [`astra_core::classify_tool_output`]
//! 2. **Retry policy** — transient errors get automatic retry with backoff
//! 3. **Alternative suggestion** — when a tool fails, suggest domain alternatives
//! 4. **Progressive escalation** — each stall nudge gets stronger consequences

use std::collections::HashMap;

// ── Error Classification ─────────────────────────────────────────────────────

/// Backward-compatible alias — all new code should use [`astra_core::ErrorKind`] directly.
pub type ErrorCategory = astra_core::ErrorKind;

/// Classify a tool error string into an actionable [`ErrorKind`].
///
/// Delegates to the canonical classifier in `astra_core`.
pub fn classify_error(error_str: &str) -> astra_core::ErrorKind {
    astra_core::classify_tool_output(error_str)
}

// ── Retry Policy ─────────────────────────────────────────────────────────────

/// Maximum retries for transient errors on a single tool call.
/// Override with `MO_MAX_TOOL_RETRIES` env var.
pub fn max_tool_retries() -> usize {
    astra_core::RuntimeLimits::global().max_tool_retries
}

/// Base delay for retry backoff (milliseconds).
/// Override with `MO_RETRY_BASE_MS` env var.
pub fn retry_base_ms() -> u64 {
    astra_core::RuntimeLimits::global().retry_base_ms
}

/// Determine if and how to retry a failed tool call.
///
/// Uses exponential backoff with random jitter to prevent thundering-herd
/// when multiple tool calls fail simultaneously.
///
/// When `retry_after_hint_ms` is `Some(ms)`, the server-requested delay is
/// honoured (clamped to `[base, MAX_RETRY_AFTER_MS]`).  Otherwise, standard
/// exponential backoff is used: `base * 2^attempt + random_jitter`.
pub fn should_retry(category: ErrorCategory, attempt: usize) -> Option<u64> {
    should_retry_with_hint(category, attempt, None)
}

/// Maximum server-requested retry delay we'll honour (30 s).
const MAX_RETRY_AFTER_MS: u64 = 30_000;

/// Like [`should_retry`], but accepts an optional `Retry-After` hint from the
/// HTTP response (in milliseconds).
pub fn should_retry_with_hint(
    category: ErrorCategory,
    attempt: usize,
    retry_after_hint_ms: Option<u64>,
) -> Option<u64> {
    if attempt >= max_tool_retries() {
        return None;
    }
    if !category.is_retryable() {
        // ToolTimeout (formerly CommandTimeout) is not retryable at the tool level
        // — the scope needs to be narrowed, not retried.
        return None;
    }
    let base = retry_base_ms();
    let jitter = if base > 1 {
        fastrand::u64(0..base / 2)
    } else {
        0
    };
    if let Some(hint) = retry_after_hint_ms {
        let clamped = hint.clamp(base, MAX_RETRY_AFTER_MS);
        Some(clamped + jitter)
    } else {
        let backoff = base.saturating_mul(1 << attempt.min(10));
        Some(backoff + jitter)
    }
}

// ── Alternative Tool Suggestion ──────────────────────────────────────────────

/// Tool equivalence groups — when one tool fails, suggest alternatives from
/// the same domain. This is generic (not tool-specific) — based on functional
/// categories.
const TOOL_GROUPS: &[&[&str]] = &[
    // Git information tools
    &[
        "git_log",
        "git_diff",
        "git_status",
        "git_blame",
        "git_file_history",
        "git_contributors",
        "git_log_search",
    ],
    // GitHub API tools
    &[
        "github_list_prs",
        "github_get_pr",
        "github_list_issues",
        "github_get_issue",
        "github_create_issue",
        "github_ci_status",
        "github_repo_stats",
    ],
    // File reading tools
    &["read_file", "grep", "glob", "list_dir"],
    // File writing tools
    &["write_file", "str_replace", "multi_edit"],
    // Memory tools
    &[
        "memory_store",
        "memory_search",
        "memory_correct",
        "memory_purge",
        "memory_profile",
    ],
    // MatrixOne tools
    &["mo_query", "mo_snapshot", "mo_branch"],
];

/// Suggest alternative tools when a tool fails.
/// Returns a list of tools in the same functional group, excluding the failed one
/// and any deprioritized tools.
pub fn suggest_alternatives(failed_tool: &str, deprioritized: &[&str]) -> Vec<String> {
    for group in TOOL_GROUPS {
        if group.contains(&failed_tool) {
            return group
                .iter()
                .filter(|&&t| t != failed_tool && !deprioritized.contains(&t))
                .map(|&t| t.to_string())
                .collect();
        }
    }
    Vec::new()
}

/// Build an error recovery message that includes classification, alternatives,
/// and actionable guidance.
pub fn build_recovery_message(
    tool_name: &str,
    error_str: &str,
    category: ErrorCategory,
    deprioritized: &[&str],
) -> String {
    let alternatives = suggest_alternatives(tool_name, deprioritized);
    let error_lower = error_str.to_lowercase();

    let mut msg = match category {
        ErrorCategory::Network
        | ErrorCategory::RateLimit
        | ErrorCategory::ServerError
        | ErrorCategory::StreamIdle
        | ErrorCategory::StreamTransport => format!(
            "⚠ {} failed with a transient error (network/timeout). \
             The system retried automatically but it still failed.",
            tool_name
        ),
        ErrorCategory::Auth => format!(
            "⚠ {} failed with an authentication/permission error. \
             Do NOT retry — check credentials or use a different approach.",
            tool_name
        ),
        ErrorCategory::ToolNotFound => format!(
            "⚠ {} failed: resource not found. \
             Verify the path/name is correct before retrying.",
            tool_name
        ),
        ErrorCategory::ToolInvalidArgs | ErrorCategory::InvalidRequest => format!(
            "⚠ {} failed: invalid arguments. \
             Check the tool's expected parameters and fix the call.",
            tool_name
        ),
        ErrorCategory::ToolUnavailable => format!(
            "⚠ {} is not available in this environment. \
             Do NOT retry — use an alternative tool.",
            tool_name
        ),
        ErrorCategory::ResourceLimit => format!(
            "⚠ {} failed: system resource limit reached (fork/memory/disk). \
             This tool is now BLOCKED for the rest of this session. \
             Do NOT retry — reduce system load or try a different approach.",
            tool_name
        ),
        ErrorCategory::ToolTimeout => format!(
            "⚠ {} timed out — the search scope is too broad. \
             Do NOT retry with the same arguments. Instead: \
             (1) search a specific subdirectory with 'path', \
             (2) use 'include' to filter file types (e.g. '*.rs'), \
             (3) use a more specific pattern.",
            tool_name
        ),
        _ => {
            astra_core::agent_debug!(
                "error_recovery",
                "unclassified_error tool={} error={}",
                tool_name,
                error_str.chars().take(200).collect::<String>()
            );
            format!(
                "⚠ {} failed with an unexpected error. Check the tool output and adjust; \
                 enable MO_DEBUG=1 or RUST_LOG for a structured log line.",
                tool_name
            )
        }
    };

    if category == ErrorCategory::ToolInvalidArgs
        && matches!(
            tool_name,
            "write_file" | "str_replace" | "multi_edit" | "apply_patch"
        )
        && astra_core::error_kind::is_workspace_read_before_write(&error_lower)
    {
        // The check_staleness error already contains the actionable "→ Action required"
        // line with the concrete file path. Avoid duplicating the guidance — just add
        // the workspace-safety framing and let the original error speak for itself.
        msg = format!(
            "⚠ {} was blocked by workspace safety: the path must be read in this session before you edit it. \
             For existing files, call read_file on that exact path first (use a full read before write_file overwrite); \
             if the file changed on disk since your last read, read it again. Then retry.",
            tool_name
        );
    }

    if !alternatives.is_empty() {
        msg.push_str(&format!(" Alternatives: [{}].", alternatives.join(", ")));
    }
    if tool_name == "read_file" && error_lower.contains("file is too large") {
        msg.push_str(
            " For large files, do NOT switch to bash. Retry read_file with \
             start_line/end_line for a narrow range, or outline=true to inspect definitions first.",
        );
    } else if tool_name == "read_file" && error_lower.contains("is a directory") {
        msg.push_str(" Use list_dir for directories instead of retrying read_file.");
    }

    msg
}

// ── Progressive Escalation ───────────────────────────────────────────────────

/// Escalation level based on accumulated problems in the session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EscalationLevel {
    /// Normal operation — hints and suggestions only.
    Normal,
    /// Warning — explicit tool avoidance list, stronger language.
    Warning,
    /// Critical — restrict available tools, force user interaction.
    Critical,
}

/// Determine escalation level from session error signals.
///
/// - `nudge_count`: how many stall nudges have been sent
/// - `total_errors`: total tool errors this session (excluding auth + timeouts)
/// - `deprioritized_count`: number of deprioritized tools
///
/// Thresholds are deliberately generous: normal agent behavior (search→read→search)
/// should NEVER trigger escalation. Only truly stuck/broken sessions escalate.
pub fn escalation_level(
    nudge_count: usize,
    total_errors: usize,
    deprioritized_count: usize,
) -> EscalationLevel {
    // Critical: nudges + errors coupled (prevents pure-stall sessions from
    // force-stopping when the agent is actually making progress with 0 errors),
    // or many errors with deprioritized tools,
    // or very high total errors (scattered failures are still broken),
    // or very high nudge count alone (the agent is spinning even without errors,
    //   e.g. cache-hit loops reading the same files repeatedly).
    //
    // Previously nudge_count >= 3 alone triggered Critical, which meant a session
    // with repeated exploration patterns (grep→read→grep) and ZERO tool errors
    // could be force-stopped. Now we require at least 3 actionable errors to
    // accompany the stall signal at the lower threshold, but a very high nudge
    // count (>= 6) alone is sufficient — at that point the agent is clearly stuck.
    //
    // Thresholds raised after Session 7875e355 diagnostic showed:
    // - str_replace errors (old_str == new_str) were escalating too fast
    // - 5 errors → Warning was too aggressive for normal retry patterns
    // - Single-tool loops should not escalate; genuine stuck-ness requires
    //   failures across multiple tools or high error counts.
    // - nudge_count alone requires 10 (not 6) to avoid false Critical on
    //   normal sessions where cache-hit nudges accumulate without errors.
    if (nudge_count >= 4 && total_errors >= 3)
        || (total_errors >= 12 && deprioritized_count >= 2)
        || total_errors >= 15
        || nudge_count >= 10
    {
        return EscalationLevel::Critical;
    }
    // Warning: 3 nudges, or 8+ errors
    if nudge_count >= 3 || total_errors >= 8 {
        return EscalationLevel::Warning;
    }
    EscalationLevel::Normal
}

/// Build an escalation message appropriate for the current level.
pub fn build_escalation_message(level: EscalationLevel, avoid_tools: &[String]) -> Option<String> {
    match level {
        EscalationLevel::Normal => None,
        EscalationLevel::Warning => {
            let mut msg = "⚠ SESSION WARNING: Multiple issues detected. Focus on completing the user's request directly.".to_string();
            if !avoid_tools.is_empty() {
                msg.push_str(&format!(" Avoid: [{}].", avoid_tools.join(", ")));
            }
            Some(msg)
        }
        EscalationLevel::Critical => Some(format!(
            "🚨 SESSION CRITICAL: Too many errors and stalls. You MUST either: \
                 (1) Answer the user with what you have so far, OR \
                 (2) Ask the user for clarification. \
                 Do NOT continue calling tools that have been failing.{}",
            if !avoid_tools.is_empty() {
                format!(" Blocked tools: [{}].", avoid_tools.join(", "))
            } else {
                String::new()
            }
        )),
    }
}

// ── Session Error Summary ────────────────────────────────────────────────────

/// Lightweight session-level error tracking for escalation decisions.
#[derive(Debug, Clone, Default)]
pub struct SessionErrorSummary {
    pub total_errors: usize,
    pub errors_by_category: HashMap<ErrorCategory, usize>,
    pub retries_performed: usize,
    pub retries_succeeded: usize,
}

impl SessionErrorSummary {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn record_error(&mut self, category: ErrorCategory) {
        self.total_errors += 1;
        *self.errors_by_category.entry(category).or_default() += 1;
    }

    pub fn record_retry(&mut self, succeeded: bool) {
        self.retries_performed += 1;
        if succeeded {
            self.retries_succeeded += 1;
        }
    }

    /// Retry success rate. Returns 1.0 if no retries performed.
    pub fn retry_success_rate(&self) -> f64 {
        if self.retries_performed == 0 {
            1.0
        } else {
            self.retries_succeeded as f64 / self.retries_performed as f64
        }
    }
}

// `ErrorCategory` is now a type alias for `ErrorKind` which already derives `Hash`.

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── Error classification ──

    #[test]
    fn classify_timeout() {
        assert_eq!(
            classify_error("connection timed out after 30s"),
            ErrorCategory::Network
        );
        assert_eq!(classify_error("ETIMEOUT"), ErrorCategory::Network);
    }

    #[test]
    fn classify_rate_limit() {
        assert_eq!(
            classify_error("rate limit exceeded (429)"),
            ErrorCategory::Network
        );
        assert_eq!(
            classify_error("HTTP 503 Service Unavailable"),
            ErrorCategory::Network
        );
    }

    #[test]
    fn classify_auth() {
        assert_eq!(classify_error("401 Unauthorized"), ErrorCategory::Auth);
        assert_eq!(
            classify_error("Permission denied: insufficient scope"),
            ErrorCategory::Auth
        );
        assert_eq!(classify_error("token expired"), ErrorCategory::Auth);
    }

    /// Regression: session f9903b97 — "Could not validate credentials" from
    /// jwt.rs was classified as Unknown, causing false stall escalation.
    #[test]
    fn classify_auth_could_not_validate() {
        assert_eq!(
            classify_error("Could not validate credentials"),
            ErrorCategory::Auth
        );
        assert_eq!(
            classify_error("could not validate credentials for user"),
            ErrorCategory::Auth
        );
    }

    #[test]
    fn classify_auth_credentials() {
        assert_eq!(classify_error("invalid credentials"), ErrorCategory::Auth);
        assert_eq!(classify_error("bad credentials"), ErrorCategory::Auth);
    }

    #[test]
    fn classify_not_found() {
        assert_eq!(classify_error("404 Not Found"), ErrorCategory::ToolNotFound);
        assert_eq!(
            classify_error("No such file or directory"),
            ErrorCategory::ToolNotFound
        );
        assert_eq!(
            classify_error("Repository does not exist"),
            ErrorCategory::ToolNotFound
        );
    }

    #[test]
    fn classify_workspace_read_before_write_guard() {
        assert_eq!(
            classify_error(
                "File exists but has not been read yet. Read it first before writing/editing."
            ),
            ErrorCategory::ToolInvalidArgs
        );
        assert_eq!(
            classify_error(
                "File was only partially read (outline or line range). Read the full file before overwriting."
            ),
            ErrorCategory::ToolInvalidArgs
        );
        assert_eq!(
            classify_error(
                "File has been modified since last read (by user or linter). Read it again before editing."
            ),
            ErrorCategory::ToolInvalidArgs
        );
    }

    /// The new actionable error messages (with "→ Action required" and concrete
    /// file paths) must still be classified as InvalidArgs so the recovery
    /// pipeline handles them correctly.
    #[test]
    fn classify_actionable_read_before_write_errors() {
        assert_eq!(
            classify_error(
                "File exists but has not been read yet. Read it first before writing/editing.\n\
                 → Action required: call read_file(\"src/main.rs\") first, then retry."
            ),
            ErrorCategory::ToolInvalidArgs
        );
        assert_eq!(
            classify_error(
                "File has been modified since last read (by user or linter). \
                 Read it again before editing.\n\
                 → Action required: call read_file(\"config.toml\") first, then retry."
            ),
            ErrorCategory::ToolInvalidArgs
        );
        assert_eq!(
            classify_error(
                "Pre-write staleness check failed: File has been modified since last read \
                 (by user or linter). Read it again before editing.\n\
                 → Action required: call read_file(\"src/lib.rs\") first, then retry."
            ),
            ErrorCategory::ToolInvalidArgs
        );
    }

    #[test]
    fn classify_invalid_args() {
        assert_eq!(
            classify_error("invalid JSON in arguments"),
            ErrorCategory::ToolInvalidArgs
        );
        assert_eq!(
            classify_error("missing required field 'path'"),
            ErrorCategory::ToolInvalidArgs
        );
        assert_eq!(
            classify_error(
                "Error: file is too large (97716 bytes, ~2442 lines). Use start_line/end_line to read a specific range, or outline=true to see definitions only."
            ),
            ErrorCategory::ToolInvalidArgs
        );
    }

    #[test]
    fn classify_unavailable() {
        assert_eq!(
            classify_error("mysql: command not found"),
            ErrorCategory::ToolUnavailable
        );
        assert_eq!(
            classify_error("Tool not configured for this environment"),
            ErrorCategory::ToolUnavailable
        );
    }

    #[test]
    fn classify_unknown() {
        assert_eq!(
            classify_error("something went wrong"),
            ErrorCategory::Unknown
        );
    }

    #[test]
    fn classify_http_500_as_transient() {
        assert_eq!(
            classify_error("HTTP 500 Internal Server Error"),
            ErrorCategory::Network
        );
        assert_eq!(
            classify_error("internal server error"),
            ErrorCategory::Network
        );
    }

    #[test]
    fn classify_http_status_aliases_as_transient() {
        assert_eq!(classify_error("502 Bad Gateway"), ErrorCategory::Network);
        assert_eq!(
            classify_error("service unavailable"),
            ErrorCategory::Network
        );
        assert_eq!(classify_error("gateway timeout"), ErrorCategory::Network);
    }

    // ── Retry policy ──

    #[test]
    fn retry_transient_first_attempt() {
        // base=500, attempt=0: backoff=500, jitter ∈ [0, 250) → [500, 750)
        let d = should_retry(ErrorCategory::Network, 0).unwrap();
        assert!((500..750).contains(&d), "attempt 0 delay={d}");
    }

    #[test]
    fn retry_transient_second_attempt() {
        // base=500, attempt=1: backoff=1000, jitter ∈ [0, 250) → [1000, 1250)
        let d = should_retry(ErrorCategory::Network, 1).unwrap();
        assert!((1000..1250).contains(&d), "attempt 1 delay={d}");
    }

    #[test]
    fn retry_transient_exhausted() {
        assert_eq!(should_retry(ErrorCategory::Network, 2), None);
    }

    #[test]
    fn no_retry_auth() {
        assert_eq!(should_retry(ErrorCategory::Auth, 0), None);
    }

    #[test]
    fn no_retry_not_found() {
        assert_eq!(should_retry(ErrorCategory::ToolNotFound, 0), None);
    }

    #[test]
    fn no_retry_invalid_args() {
        assert_eq!(should_retry(ErrorCategory::ToolInvalidArgs, 0), None);
    }

    // ── Alternative suggestions ──

    #[test]
    fn suggest_git_alternatives() {
        let alts = suggest_alternatives("git_log", &[]);
        assert!(alts.contains(&"git_diff".to_string()));
        assert!(alts.contains(&"git_blame".to_string()));
        assert!(!alts.contains(&"git_log".to_string())); // excludes self
    }

    #[test]
    fn suggest_excludes_deprioritized() {
        let alts = suggest_alternatives("git_log", &["git_diff", "git_blame"]);
        assert!(!alts.contains(&"git_diff".to_string()));
        assert!(!alts.contains(&"git_blame".to_string()));
        assert!(alts.contains(&"git_status".to_string()));
    }

    #[test]
    fn suggest_no_alternatives_for_unique_tool() {
        let alts = suggest_alternatives("bash", &[]);
        assert!(alts.is_empty());
    }

    #[test]
    fn suggest_github_alternatives() {
        let alts = suggest_alternatives("github_list_prs", &[]);
        assert!(alts.contains(&"github_get_pr".to_string()));
    }

    #[test]
    fn suggest_memory_alternatives() {
        let alts = suggest_alternatives("memory_store", &[]);
        assert!(alts.contains(&"memory_search".to_string()));
    }

    #[test]
    fn suggest_multi_edit_alternatives() {
        let alts = suggest_alternatives("multi_edit", &[]);
        assert!(alts.contains(&"write_file".to_string()));
        assert!(alts.contains(&"str_replace".to_string()));
    }

    // ── Recovery message ──

    #[test]
    fn recovery_message_includes_alternatives() {
        let msg = build_recovery_message("git_log", "timeout", ErrorCategory::Network, &[]);
        assert!(msg.contains("git_diff"));
        assert!(msg.contains("Alternatives"));
    }

    #[test]
    fn recovery_message_write_file_read_guard_is_actionable() {
        let err = "File exists but has not been read yet. Read it first before writing/editing.";
        let cat = classify_error(err);
        assert_eq!(cat, ErrorCategory::ToolInvalidArgs);
        let msg = build_recovery_message("write_file", err, cat, &[]);
        assert!(msg.contains("read_file"));
        assert!(msg.contains("workspace safety"));
    }

    #[test]
    fn recovery_message_auth_no_retry() {
        let msg = build_recovery_message("github_list_prs", "401", ErrorCategory::Auth, &[]);
        assert!(msg.contains("authentication"));
        assert!(msg.contains("Do NOT retry"));
    }

    #[test]
    fn recovery_message_unavailable() {
        let msg = build_recovery_message(
            "mo_query",
            "not installed",
            ErrorCategory::ToolUnavailable,
            &[],
        );
        assert!(msg.contains("not available"));
        assert!(msg.contains("alternative"));
    }

    #[test]
    fn recovery_message_guides_large_read_file_back_to_read_file() {
        let msg = build_recovery_message(
            "read_file",
            "Error: file is too large (97716 bytes, ~2442 lines). Use start_line/end_line to read a specific range, or outline=true to see definitions only.",
            ErrorCategory::Unknown,
            &[],
        );
        assert!(msg.contains("do NOT switch to bash"));
        assert!(msg.contains("start_line/end_line"));
        assert!(msg.contains("outline=true"));
    }

    // ── Escalation ──

    #[test]
    fn escalation_normal_fresh_session() {
        assert_eq!(escalation_level(0, 0, 0), EscalationLevel::Normal);
    }

    #[test]
    fn escalation_normal_low_nudges() {
        // 1 nudge: still Normal
        assert_eq!(escalation_level(1, 0, 0), EscalationLevel::Normal);
    }

    #[test]
    fn escalation_warning_three_nudges() {
        // 3 nudges → Warning (raised from 2 to reduce false positives)
        assert_eq!(escalation_level(3, 0, 0), EscalationLevel::Warning);
    }

    #[test]
    fn escalation_two_nudges_is_normal() {
        // 2 nudges alone → Normal (threshold raised after Session 7875e355)
        assert_eq!(escalation_level(2, 0, 0), EscalationLevel::Normal);
    }

    #[test]
    fn escalation_critical_four_nudges_with_errors() {
        // 4 nudges alone (0 errors) → Warning, NOT Critical.
        // Critical requires nudge_count >= 4 AND total_errors >= 3.
        // This prevents force-stopping sessions with exploration patterns
        // (grep→read→grep) that produce stall nudges but zero tool errors.
        assert_eq!(escalation_level(4, 0, 0), EscalationLevel::Warning);
        assert_eq!(escalation_level(5, 0, 0), EscalationLevel::Warning);
        // But with 3+ errors, nudges trigger Critical
        assert_eq!(escalation_level(4, 3, 0), EscalationLevel::Critical);
        assert_eq!(escalation_level(5, 4, 0), EscalationLevel::Critical);
    }

    #[test]
    fn escalation_warning_eight_errors() {
        // 8 errors → Warning (raised from 5 after Session 7875e355)
        assert_eq!(escalation_level(0, 8, 0), EscalationLevel::Warning);
    }

    #[test]
    fn escalation_normal_few_errors() {
        // 3-7 errors: still Normal (raised thresholds)
        assert_eq!(escalation_level(0, 3, 0), EscalationLevel::Normal);
        assert_eq!(escalation_level(0, 5, 0), EscalationLevel::Normal);
        assert_eq!(escalation_level(0, 7, 0), EscalationLevel::Normal);
    }

    #[test]
    fn escalation_critical_from_nudges() {
        // Nudges alone stay Warning; coupled with errors → Critical
        assert_eq!(escalation_level(4, 0, 0), EscalationLevel::Warning);
        assert_eq!(escalation_level(5, 0, 0), EscalationLevel::Warning);
        assert_eq!(escalation_level(4, 3, 0), EscalationLevel::Critical);
        assert_eq!(escalation_level(5, 4, 0), EscalationLevel::Critical);
    }

    #[test]
    fn escalation_critical_many_errors_with_deprioritized() {
        // 12+ errors with at least 2 deprioritized tools → Critical (raised thresholds)
        assert_eq!(escalation_level(0, 12, 2), EscalationLevel::Critical);
        assert_eq!(escalation_level(0, 13, 3), EscalationLevel::Critical);
    }

    #[test]
    fn escalation_not_critical_errors_without_enough_deprioritized() {
        // 12 errors but only 1 deprioritized tool → Warning, not Critical
        assert_eq!(escalation_level(0, 12, 1), EscalationLevel::Warning);
    }

    #[test]
    fn escalation_critical_fifteen_errors_regardless() {
        // 15+ errors with zero deprioritized → Critical (new: standalone high-error gate)
        assert_eq!(escalation_level(0, 15, 0), EscalationLevel::Critical);
        assert_eq!(escalation_level(0, 18, 0), EscalationLevel::Critical);
    }

    #[test]
    fn escalation_fourteen_errors_no_deprioritized_is_warning() {
        // Below the standalone threshold, no deprioritized → stays Warning
        assert_eq!(escalation_level(0, 14, 0), EscalationLevel::Warning);
    }

    #[test]
    fn escalation_message_normal_is_none() {
        assert!(build_escalation_message(EscalationLevel::Normal, &[]).is_none());
    }

    #[test]
    fn escalation_message_warning_has_content() {
        let msg = build_escalation_message(EscalationLevel::Warning, &["bash".to_string()]);
        assert!(msg.is_some());
        assert!(msg.unwrap().contains("bash"));
    }

    #[test]
    fn escalation_message_critical_has_must() {
        let msg = build_escalation_message(EscalationLevel::Critical, &[]);
        assert!(msg.is_some());
        assert!(msg.unwrap().contains("MUST"));
    }

    // ── Session error summary ──

    #[test]
    fn error_summary_tracks_categories() {
        let mut summary = SessionErrorSummary::new();
        summary.record_error(ErrorCategory::Network);
        summary.record_error(ErrorCategory::Network);
        summary.record_error(ErrorCategory::Auth);
        assert_eq!(summary.total_errors, 3);
        assert_eq!(summary.errors_by_category[&ErrorCategory::Network], 2);
        assert_eq!(summary.errors_by_category[&ErrorCategory::Auth], 1);
    }

    #[test]
    fn retry_success_rate_no_retries() {
        let summary = SessionErrorSummary::new();
        assert_eq!(summary.retry_success_rate(), 1.0);
    }

    #[test]
    fn retry_success_rate_mixed() {
        let mut summary = SessionErrorSummary::new();
        summary.record_retry(true);
        summary.record_retry(false);
        summary.record_retry(true);
        assert!((summary.retry_success_rate() - 0.6667).abs() < 0.01);
    }

    // ── ResourceLimit classification tests ──

    #[test]
    fn classify_fork_resource_limit() {
        assert_eq!(
            classify_error("Error: fork: Resource temporarily unavailable"),
            ErrorCategory::ResourceLimit
        );
    }

    #[test]
    fn classify_oom_resource_limit() {
        assert_eq!(
            classify_error("Error: Cannot allocate memory"),
            ErrorCategory::ResourceLimit
        );
    }

    #[test]
    fn classify_disk_full_resource_limit() {
        assert_eq!(
            classify_error("Error: No space left on device"),
            ErrorCategory::ResourceLimit
        );
    }

    #[test]
    fn classify_chinese_resource_limit() {
        assert_eq!(
            classify_error("Error: 系统资源暂时不足，无法运行"),
            ErrorCategory::ResourceLimit
        );
    }

    #[test]
    fn classify_too_many_open_files() {
        assert_eq!(
            classify_error("Error: too many open files"),
            ErrorCategory::ResourceLimit
        );
    }

    #[test]
    fn resource_limit_not_transient() {
        // Make sure resource limit is NOT classified as Transient
        // even though it contains "unavailable" (which Unavailable would match)
        let cat = classify_error("fork: Resource temporarily unavailable");
        assert_eq!(cat, ErrorCategory::ResourceLimit);
        assert_ne!(cat, ErrorCategory::Network);
    }

    // ── CommandTimeout classification ──

    #[test]
    fn classify_command_timeout_grep() {
        assert_eq!(
            classify_error("Error: grep timed out after 30s with no results"),
            ErrorCategory::ToolTimeout
        );
    }

    #[test]
    fn classify_command_timeout_generic() {
        assert_eq!(
            classify_error("Error: command timed out after 30s"),
            ErrorCategory::ToolTimeout
        );
    }

    #[test]
    fn classify_command_timeout_not_connection() {
        // "connection timed out" should still be Transient, not CommandTimeout
        assert_eq!(
            classify_error("connection timed out after 30s"),
            ErrorCategory::Network
        );
    }

    #[test]
    fn no_retry_command_timeout() {
        assert_eq!(should_retry(ErrorCategory::ToolTimeout, 0), None);
    }

    #[test]
    fn recovery_message_command_timeout_has_guidance() {
        let msg = build_recovery_message(
            "grep",
            "Error: grep timed out after 30s",
            ErrorCategory::ToolTimeout,
            &[],
        );
        assert!(msg.contains("timed out"), "got: {msg}");
        assert!(
            msg.contains("path"),
            "should suggest narrowing path, got: {msg}"
        );
        assert!(
            msg.contains("include"),
            "should suggest include filter, got: {msg}"
        );
        assert!(
            !msg.contains("retried"),
            "should NOT mention retry, got: {msg}"
        );
    }

    #[test]
    fn recovery_message_for_resource_limit() {
        let msg = build_recovery_message(
            "bash",
            "fork: Resource temporarily unavailable",
            ErrorCategory::ResourceLimit,
            &[],
        );
        assert!(msg.contains("BLOCKED"));
        assert!(msg.contains("resource limit"));
    }

    // ── New error pattern classification tests ──

    #[test]
    fn classify_enospc_resource_limit() {
        assert_eq!(
            classify_error("write failed: ENOSPC"),
            ErrorCategory::ResourceLimit
        );
    }

    #[test]
    fn classify_ebusy_resource_limit() {
        assert_eq!(
            classify_error("Device or resource busy"),
            ErrorCategory::ResourceLimit
        );
    }

    #[test]
    fn classify_chinese_oom_resource_limit() {
        assert_eq!(
            classify_error("Error: 内存不足，无法继续"),
            ErrorCategory::ResourceLimit
        );
    }

    #[test]
    fn classify_eacces_as_auth() {
        assert_eq!(
            classify_error("EACCES: permission denied, open '/etc/shadow'"),
            ErrorCategory::Auth
        );
    }

    #[test]
    fn classify_eperm_as_auth() {
        assert_eq!(
            classify_error("EPERM: operation not permitted"),
            ErrorCategory::Auth
        );
    }

    #[test]
    fn classify_operation_not_permitted_as_auth() {
        assert_eq!(
            classify_error("Error: Operation not permitted"),
            ErrorCategory::Auth
        );
    }

    #[test]
    fn classify_eisdir_as_not_found() {
        assert_eq!(
            classify_error("Error: EISDIR: illegal operation on a directory"),
            ErrorCategory::ToolNotFound
        );
    }

    #[test]
    fn classify_is_a_directory_as_not_found() {
        assert_eq!(
            classify_error("Error: Is a directory"),
            ErrorCategory::ToolNotFound
        );
    }

    // ── should_retry_with_hint tests ──

    #[test]
    fn retry_with_hint_honours_server_delay() {
        let delay = should_retry_with_hint(ErrorCategory::Network, 0, Some(5000)).unwrap();
        // hint=5000ms, clamped to [500, 30000], plus jitter [0, 250)
        assert!((5000..5250).contains(&delay), "delay={delay}");
    }

    #[test]
    fn retry_with_hint_clamps_low_to_base() {
        // Server says 50ms but base is 500ms → clamped up to 500
        let delay = should_retry_with_hint(ErrorCategory::Network, 0, Some(50)).unwrap();
        assert!((500..750).contains(&delay), "delay={delay}");
    }

    #[test]
    fn retry_with_hint_clamps_high_to_max() {
        // Server says 60s but max is 30s → clamped down to 30000
        let delay = should_retry_with_hint(ErrorCategory::Network, 0, Some(60_000)).unwrap();
        assert!((30_000..30_250).contains(&delay), "delay={delay}");
    }

    #[test]
    fn retry_with_hint_none_uses_exponential() {
        let delay = should_retry_with_hint(ErrorCategory::Network, 0, None).unwrap();
        // Same as should_retry: base=500 + jitter [0, 250) → [500, 750)
        assert!((500..750).contains(&delay), "delay={delay}");
    }

    #[test]
    fn retry_with_hint_respects_max_attempts() {
        assert!(should_retry_with_hint(ErrorCategory::Network, 2, Some(1000)).is_none());
    }

    #[test]
    fn retry_with_hint_non_transient_never_retries() {
        assert!(should_retry_with_hint(ErrorCategory::Auth, 0, Some(1000)).is_none());
        assert!(should_retry_with_hint(ErrorCategory::ResourceLimit, 0, Some(1000)).is_none());
    }

    #[test]
    fn retry_jitter_is_non_deterministic() {
        // Run 20 retries — if jitter is truly random, we should see at least 2 distinct values
        let delays: Vec<u64> = (0..20)
            .map(|_| should_retry(ErrorCategory::Network, 0).unwrap())
            .collect();
        let unique: std::collections::HashSet<u64> = delays.iter().copied().collect();
        assert!(
            unique.len() >= 2,
            "expected non-deterministic jitter, got {unique:?}"
        );
    }
}
