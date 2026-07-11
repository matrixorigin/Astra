//! Error classification, retry policy, and alternative tool suggestion.
//!
//! Provides a systematic approach to non-happy-path handling:
//! 1. **Error classification** — delegates to [`astra_core::classify_tool_output`]
//! 2. **Retry policy** — transient errors get automatic retry with backoff
//! 3. **Alternative suggestion** — when a tool fails, suggest domain alternatives
//! 4. **Progressive escalation** — each stall nudge gets stronger consequences

use std::collections::{HashMap, VecDeque};

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
/// Override with `ASTRA_MAX_TOOL_RETRIES` env var.
pub fn max_tool_retries() -> usize {
    astra_core::RuntimeLimits::global().max_tool_retries
}

/// Base delay for retry backoff (milliseconds).
/// Override with `ASTRA_RETRY_BASE_MS` env var.
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
    &["git"],
    // GitHub API tools
    &["github"],
    // File reading tools
    &["read_file", "grep", "glob", "list_dir"],
    // File writing tools
    &["write_file", "str_replace", "multi_edit"],
    // Memory tool is single-row and action-aware; no peer alternatives.
    // MatrixOne tools
    &["mo_query"],
];

/// Suggest alternative tools when a tool fails.
/// Returns a list of tools in the same functional group, excluding the failed one
/// and any health-avoidance tools.
pub fn suggest_alternatives(failed_tool: &str, avoidance_advised: &[&str]) -> Vec<String> {
    for group in TOOL_GROUPS {
        if group.contains(&failed_tool) {
            return group
                .iter()
                .filter(|&&t| t != failed_tool && !avoidance_advised.contains(&t))
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
    avoidance_advised: &[&str],
) -> String {
    build_recovery_message_with_evidence(tool_name, error_str, category, avoidance_advised, None)
}

pub fn build_recovery_message_with_evidence(
    tool_name: &str,
    error_str: &str,
    category: ErrorCategory,
    avoidance_advised: &[&str],
    evidence: Option<&astra_core::ToolFailureEvidence>,
) -> String {
    if let Some(evidence) = evidence {
        match evidence.cause {
            astra_core::ToolFailureCause::InputTooLarge => {
                return format!(
                    "⚠ {tool_name} rejected an oversized input. Use a targeted line/range read, narrow the path or query, or search for the relevant location before reading full content. This is structured recovery evidence; do not repeat the identical call."
                );
            }
            astra_core::ToolFailureCause::ScopeTooBroad => {
                return format!(
                    "⚠ {tool_name} could not complete the requested scope. Narrow the directory, file type, query, or target and then make a corrected call."
                );
            }
            _ => {}
        }
    }
    let alternatives = suggest_alternatives(tool_name, avoidance_advised);
    let ask_user_invalid_args = tool_name == "ask_user"
        && matches!(
            category,
            ErrorCategory::ToolInvalidArgs | ErrorCategory::InvalidRequest
        );
    let write_file_invalid_args =
        tool_name == "write_file" && category == ErrorCategory::ToolInvalidArgs;
    let file_edit_invalid_args = matches!(
        tool_name,
        "write_file" | "str_replace" | "multi_edit" | "apply_patch"
    ) && matches!(
        category,
        ErrorCategory::ToolInvalidArgs | ErrorCategory::InvalidRequest
    );
    let task_board_invalid_args = tool_name == "task_board"
        && matches!(
            category,
            ErrorCategory::ToolInvalidArgs | ErrorCategory::InvalidRequest
        );

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
        ErrorCategory::ToolInvalidArgs | ErrorCategory::InvalidRequest => {
            if ask_user_invalid_args {
                "⚠ ask_user failed: invalid questionnaire arguments. You chose ask_user because user clarification is required. Retry the SAME ask_user tool immediately with corrected questionnaire args. Do NOT continue implementation, guess defaults, or act as if the user already answered. Use a top-level `questions` array, for example: {\"questions\":[{\"header\":\"Scope\",\"question\":\"Which scope should we ship first?\",\"options\":[\"Core flow\",\"Full workflow\"],\"allow_freeform\":true}]}.".to_string()
            } else if write_file_invalid_args {
                "⚠ write_file failed: invalid arguments or workspace safety precondition. Retry the same tool with both `path` and `content` for writes, or `path` + `delete=true` for deletes. For existing files, call read_file on the exact path in this session before editing it. Do NOT switch to bash or python just to write or delete this file.".to_string()
            } else if task_board_invalid_args {
                astra_tools::task_tool_contract::task_invalid_args_recovery_message()
            } else if file_edit_invalid_args {
                format!(
                    "⚠ {} failed: invalid file-edit arguments or workspace safety precondition. \
                     Retry the same tool with the required structured arguments. For existing files, call read_file on the exact path in this session before editing it. \
                     Do NOT switch to bash or python just to edit files.",
                    tool_name
                )
            } else {
                format!(
                    "⚠ {} failed: invalid arguments. \
                     Check the tool's expected parameters, fix the call, and retry the same tool before switching approaches.",
                    tool_name
                )
            }
        }
        ErrorCategory::ToolUnavailable => format!(
            "⚠ {} is not available in this environment. \
             Do NOT retry — use an alternative tool.",
            tool_name
        ),
        ErrorCategory::ToolBinding => format!(
            "⚠ {} was advertised but no executor/transport was bound for this turn. \
             Do NOT retry the same call. Do NOT assume bash or a different tool is equivalent; \
             continue with degraded coverage only after stating what capability was lost.",
            tool_name
        ),
        ErrorCategory::ResourceLimit => format!(
            "⚠ {} failed: system resource limit reached (fork/memory/disk). \
             This tool is now BLOCKED for the rest of this session. \
             Do NOT retry — reduce system load or try a different approach.",
            tool_name
        ),
        ErrorCategory::ToolTimeout => {
            if matches!(tool_name, "grep" | "glob") {
                format!(
                    "⚠ {} timed out — the search scope is too broad. \
                     Do NOT retry with the same arguments. Instead: \
                     (1) search a specific subdirectory with 'path', \
                     (2) use 'include' to filter file types (e.g. '*.rs'), \
                     (3) use a more specific pattern.",
                    tool_name
                )
            } else {
                format!(
                    "⚠ {} timed out. Do NOT retry the identical long-running command. \
                     Instead run a narrower target, increase the timeout only when the command is expected to be slow, \
                     or split build/test work into focused commands.",
                    tool_name
                )
            }
        }
        _ => {
            if ask_user_invalid_args {
                "⚠ ask_user failed: invalid questionnaire arguments. You chose ask_user because user clarification is required. Retry the SAME ask_user tool immediately with corrected questionnaire args. Do NOT continue implementation, guess defaults, or act as if the user already answered. Use a top-level `questions` array, for example: {\"questions\":[{\"header\":\"Scope\",\"question\":\"Which scope should we ship first?\",\"options\":[\"Core flow\",\"Full workflow\"],\"allow_freeform\":true}]}.".to_string()
            } else {
                astra_core::agent_debug!(
                    "error_recovery",
                    "unclassified_error tool={} error={}",
                    tool_name,
                    error_str.chars().take(200).collect::<String>()
                );
                format!(
                    "⚠ {} failed with an unclassified tool error. Do NOT retry the identical call. \
                     Use structured facts first: verify the tool name, required arguments, selected provider/runtime, and workspace preconditions; \
                     retry only with corrected arguments or switch to a capability-equivalent tool that is visible in the current surface.",
                    tool_name
                )
            }
        }
    };

    if tool_name == "str_replace" && category == ErrorCategory::ToolInvalidArgs {
        msg = format!(
            "⚠ {tool_name} failed: invalid replacement arguments or stale file context. \
             Do NOT retry str_replace with the same arguments. \
             Call read_file on the target file first (use a targeted line range for large files), \
             copy the exact current bytes, then retry str_replace.",
        );
    }

    if !alternatives.is_empty() {
        const SHELL_TOOLS: &[&str] = &["bash", "powershell"];
        let filtered: Vec<&str> = if write_file_invalid_args {
            // Missing-args errors should steer the model to retry write_file, but legitimate
            // editing alternatives (str_replace, multi_edit) must still be visible.
            // Only suppress shell escalation paths.
            alternatives
                .iter()
                .map(String::as_str)
                .filter(|t| !SHELL_TOOLS.contains(t))
                .collect()
        } else {
            alternatives.iter().map(String::as_str).collect()
        };
        if !filtered.is_empty() {
            msg.push_str(&format!(" Alternatives: [{}].", filtered.join(", ")));
        }
    }
    if tool_name == "read_file" && category == ErrorCategory::ResourceLimit {
        msg.push_str("\n\nThe read target is too large for one call. Use start_line/end_line, a narrower path, or a search tool before reading the full content. do NOT switch to bash just to read it.");
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
/// - `health_avoidance_count`: number of health-avoidance tools
///
/// Thresholds are deliberately generous: normal agent behavior (search→read→search)
/// should NEVER trigger escalation. Only truly stuck/broken sessions escalate.
pub fn escalation_level(
    nudge_count: usize,
    total_errors: usize,
    health_avoidance_count: usize,
) -> EscalationLevel {
    // Critical: nudges + errors coupled (prevents pure-stall sessions from
    // force-stopping when the agent is actually making progress with 0 errors),
    // or many errors with health-avoidance tools,
    // or very high total errors (scattered failures are still broken),
    // or very high nudge count alone (the agent is spinning even without errors).
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
    //   normal sessions with repeated exploration but no tool errors.
    if (nudge_count >= 4 && total_errors >= 3)
        || (total_errors >= 12 && health_avoidance_count >= 2)
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
                msg.push_str(&format!(
                    " Retry-cautioned tools: [{}]; change inputs or strategy before using them again.",
                    avoid_tools.join(", ")
                ));
            }
            Some(msg)
        }
        EscalationLevel::Critical => Some(format!(
            "🚨 SESSION CRITICAL: Too many errors and stalls. You MUST either: \
                 (1) Answer the user with what you have so far, OR \
                 (2) Ask the user for clarification. \
                 Do NOT continue calling tools that have been failing.{}",
            if !avoid_tools.is_empty() {
                format!(" Retry-cautioned tools: [{}].", avoid_tools.join(", "))
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
    /// Lifetime session error count for telemetry and diagnostics.
    pub total_errors: usize,
    /// Lifetime category counts aligned with `total_errors`.
    pub errors_by_category: HashMap<ErrorCategory, usize>,
    pub retries_performed: usize,
    pub retries_succeeded: usize,
    recent_total_errors: usize,
    recent_errors_by_category: HashMap<ErrorCategory, usize>,
    recent_errors: VecDeque<ErrorCategory>,
}

impl SessionErrorSummary {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn record_error(&mut self, category: ErrorCategory) {
        const RECENT_ERROR_WINDOW: usize = 16;

        self.total_errors += 1;
        *self.errors_by_category.entry(category).or_default() += 1;
        self.recent_total_errors += 1;
        *self.recent_errors_by_category.entry(category).or_default() += 1;
        self.recent_errors.push_back(category);

        while self.recent_errors.len() > RECENT_ERROR_WINDOW {
            self.discard_oldest_recent_error();
        }
    }

    pub fn record_retry(&mut self, succeeded: bool) {
        self.retries_performed += 1;
        if succeeded {
            self.retries_succeeded += 1;
            self.record_success();
        }
    }

    pub fn record_success(&mut self) {
        self.discard_oldest_recent_error();
    }

    pub fn clear_recent_pressure(&mut self) {
        self.recent_total_errors = 0;
        self.recent_errors_by_category.clear();
        self.recent_errors.clear();
    }

    pub fn recent_error_pressure(&self) -> usize {
        self.recent_total_errors
    }

    pub fn recent_error_count(&self, category: ErrorCategory) -> usize {
        self.recent_errors_by_category
            .get(&category)
            .copied()
            .unwrap_or(0)
    }

    /// Retry success rate. Returns 1.0 if no retries performed.
    pub fn retry_success_rate(&self) -> f64 {
        if self.retries_performed == 0 {
            1.0
        } else {
            self.retries_succeeded as f64 / self.retries_performed as f64
        }
    }

    fn discard_oldest_recent_error(&mut self) {
        let Some(category) = self.recent_errors.pop_front() else {
            return;
        };

        self.recent_total_errors = self.recent_total_errors.saturating_sub(1);
        let mut remove_category = false;
        if let Some(count) = self.recent_errors_by_category.get_mut(&category) {
            *count = count.saturating_sub(1);
            remove_category = *count == 0;
        }
        if remove_category {
            self.recent_errors_by_category.remove(&category);
        }
    }
}

// `ErrorCategory` is now a type alias for `ErrorKind` which already derives `Hash`.

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── Classification: data-driven ──

    /// Covers timeout, rate_limit, auth variants, not_found, resource_limit, http_500, misc
    #[test]
    fn classify_errors() {
        let cases: &[(&str, ErrorCategory)] = &[
            // timeout / network / ToolTimeout
            ("connection timed out after 30s", ErrorCategory::Network),
            ("ETIMEOUT", ErrorCategory::Network),
            (
                "Error: command timed out after 30s",
                ErrorCategory::ToolTimeout,
            ),
            (
                "Error: grep timed out after 30s with no results",
                ErrorCategory::ToolTimeout,
            ),
            // rate_limit / network
            ("rate limit exceeded (429)", ErrorCategory::Network),
            ("HTTP 503 Service Unavailable", ErrorCategory::Network),
            // auth (6 original tests merged)
            ("401 Unauthorized", ErrorCategory::Auth),
            ("Permission denied: insufficient scope", ErrorCategory::Auth),
            ("token expired", ErrorCategory::Auth),
            ("Could not validate credentials", ErrorCategory::Auth),
            ("missing authentication token", ErrorCategory::Auth),
            (
                "EACCES: permission denied, open '/etc/shadow'",
                ErrorCategory::Auth,
            ),
            ("EPERM: operation not permitted", ErrorCategory::Auth),
            ("Error: Operation not permitted", ErrorCategory::Auth),
            // not_found (3 original tests merged)
            (
                "No such file: /tmp/missing.txt",
                ErrorCategory::ToolNotFound,
            ),
            ("File not found: src/main.rs", ErrorCategory::ToolNotFound),
            (
                "Error: EISDIR: illegal operation on a directory",
                ErrorCategory::ToolNotFound,
            ),
            ("Error: Is a directory", ErrorCategory::ToolNotFound),
            // resource_limit (8 original tests merged)
            (
                "Error: fork: Resource temporarily unavailable",
                ErrorCategory::ResourceLimit,
            ),
            (
                "Error: Cannot allocate memory",
                ErrorCategory::ResourceLimit,
            ),
            (
                "Error: No space left on device",
                ErrorCategory::ResourceLimit,
            ),
            (
                "Error: 系统资源暂时不足，无法运行",
                ErrorCategory::ResourceLimit,
            ),
            ("Error: too many open files", ErrorCategory::ResourceLimit),
            ("write failed: ENOSPC", ErrorCategory::ResourceLimit),
            ("Device or resource busy", ErrorCategory::ResourceLimit),
            ("Error: 内存不足，无法继续", ErrorCategory::ResourceLimit),
            // http_500 (2 original tests merged)
            ("HTTP 500 Internal Server Error", ErrorCategory::Network),
            ("HTTP 502 Bad Gateway", ErrorCategory::Network),
            // invalid_args / unknown / unavailable misc
            ("Error: Invalid argument", ErrorCategory::ToolInvalidArgs),
            ("Error: Connection refused", ErrorCategory::Network),
            ("some-random-unclassifiable-error", ErrorCategory::Unknown),
        ];
        for (msg, expected) in cases {
            assert_eq!(classify_error(msg), *expected, "for: {msg}");
        }
    }

    /// Resource limit is not transient / Network
    #[test]
    fn resource_limit_not_transient() {
        let cat = classify_error("fork: Resource temporarily unavailable");
        assert_eq!(cat, ErrorCategory::ResourceLimit);
        assert_ne!(cat, ErrorCategory::Network);
    }

    // ── Read-before-write guard (2 original tests merged) ──

    #[test]
    fn classify_read_before_write_guard() {
        for err in [
            "File exists but has not been read yet. Read it first before writing/editing.",
            "File was only partially read (outline or line range). Read the full file before overwriting.",
            "File has been modified since last read (by user or linter). Read it again before editing.",
            "Pre-write staleness check failed: File has been modified since last read (by user or linter). Read it again before editing.",
        ] {
            assert_eq!(
                classify_error(err),
                ErrorCategory::ToolInvalidArgs,
                "for: {err}"
            );
        }
    }

    // ── Retry: transient ──

    #[test]
    fn retry_transient_first_second_exhausted() {
        let d = should_retry(ErrorCategory::Network, 0).unwrap();
        assert!((500..750).contains(&d), "attempt 0 delay={d}");
        let d = should_retry(ErrorCategory::Network, 1).unwrap();
        assert!((1000..1250).contains(&d), "attempt 1 delay={d}");
        assert_eq!(should_retry(ErrorCategory::Network, 2), None);
    }

    // ── No retry for non-transient (4 original tests merged) ──

    #[test]
    fn no_retry_for_non_transient_categories() {
        for cat in [
            ErrorCategory::Auth,
            ErrorCategory::ToolNotFound,
            ErrorCategory::ToolInvalidArgs,
            ErrorCategory::ToolTimeout,
        ] {
            assert_eq!(should_retry(cat, 0), None, "should not retry {cat:?}");
        }
    }

    // ── Retry with hint (6 original tests merged) ──

    #[test]
    fn retry_with_hint_behaviour() {
        // honours server delay (hint=5000, attempt=0 → 5000..5250)
        let delay = should_retry_with_hint(ErrorCategory::Network, 0, Some(5000)).unwrap();
        assert!((5000..5250).contains(&delay), "server delay, got {delay}");
        // none hint uses exponential (attempt=0, base=500 + jitter → 500..750)
        let delay = should_retry_with_hint(ErrorCategory::Network, 0, None).unwrap();
        assert!((500..750).contains(&delay), "exponential, got {delay}");
        // clamps low to base (hint=50 → clamped to 500)
        let delay = should_retry_with_hint(ErrorCategory::Network, 0, Some(50)).unwrap();
        assert!((500..750).contains(&delay), "clamped low, got {delay}");
        // clamps high to max (hint=60s → clamped to 30s)
        let delay = should_retry_with_hint(ErrorCategory::Network, 0, Some(60_000)).unwrap();
        assert!(
            (30_000..30_250).contains(&delay),
            "clamped high, got {delay}"
        );
        // respects max attempts
        assert!(should_retry_with_hint(ErrorCategory::Network, 2, Some(1000)).is_none());
        // non-transient never retries
        assert!(should_retry_with_hint(ErrorCategory::Auth, 0, Some(1000)).is_none());
        assert!(should_retry_with_hint(ErrorCategory::ResourceLimit, 0, Some(1000)).is_none());
    }

    // ── Suggestions (5 original tests merged) ──

    #[test]
    fn suggestions_data_driven() {
        // consolidated action tools do not suggest helper-style names
        assert!(suggest_alternatives("git", &[]).is_empty());
        assert!(suggest_alternatives("github", &[]).is_empty());

        // no alternatives for unique tools
        assert!(suggest_alternatives("bash", &[]).is_empty());

        // multi-edit alternatives
        let alts = suggest_alternatives("multi_edit", &[]);
        assert!(alts.contains(&"write_file".to_string()));
        assert!(alts.contains(&"str_replace".to_string()));
    }

    // ── Recovery messages (8 original tests merged into 3) ──

    #[test]
    fn recovery_message_includes_alternatives() {
        let msg = build_recovery_message("multi_edit", "timeout", ErrorCategory::Network, &[]);
        assert!(msg.contains("write_file"));
        assert!(msg.contains("Alternatives"));
    }

    #[test]
    fn recovery_message_read_before_write_and_auth() {
        // read_before_write guard
        let err = "File exists but has not been read yet. Read it first before writing/editing.";
        let cat = classify_error(err);
        assert_eq!(cat, ErrorCategory::ToolInvalidArgs);
        let msg = build_recovery_message("write_file", err, cat, &[]);
        assert!(msg.contains("read_file"));
        assert!(msg.contains("workspace safety"));

        // auth no retry
        let msg = build_recovery_message("github", "401", ErrorCategory::Auth, &[]);
        assert!(msg.contains("authentication"));
        assert!(msg.contains("Do NOT retry"));

        // large file guidance
        let msg = build_recovery_message(
            "read_file",
            "structured resource limit",
            ErrorCategory::ResourceLimit,
            &[],
        );
        assert!(msg.contains("do NOT switch to bash"));
        assert!(msg.contains("start_line/end_line"));
    }

    #[test]
    fn recovery_message_tool_specific() {
        // unavailable tool
        let msg = build_recovery_message(
            "mo_query",
            "not installed",
            ErrorCategory::ToolUnavailable,
            &[],
        );
        assert!(msg.contains("not available"));
        assert!(msg.contains("alternative"));

        // write_file missing path disallows shell fallback
        let err = "Error: Missing 'path' parameter. Retry write_file with both path and content. Do not switch to bash or python just to write this file.";
        let cat = classify_error(err);
        assert_eq!(cat, ErrorCategory::ToolInvalidArgs);
        let msg = build_recovery_message("write_file", err, cat, &[]);
        assert!(msg.contains("bash"), "must warn against bash fallback");
        assert!(msg.contains("python"), "must warn against python fallback");
        let alts_section = msg.find("Alternatives:").map(|i| &msg[i..]).unwrap_or("");
        assert!(
            !alts_section.to_lowercase().contains("bash"),
            "bash must not be an alternative"
        );

        // task missing/invalid action → retry the same structured tool with
        // the shared action contract instead of switching tools or answering
        // as if task management succeeded.
        let err = "Error: missing required parameter `action` for `task_board`.";
        let cat = classify_error(err);
        assert_eq!(cat, ErrorCategory::ToolInvalidArgs);
        let msg = build_recovery_message("task_board", err, cat, &[]);
        assert!(msg.contains("Retry the same `task_board` tool"));
        assert!(msg.contains(astra_tools::task_tool_contract::TASK_ACTIONS_DISPLAY));
        assert!(msg.contains("use only fields allowed for that action"));

        let err = astra_tools::task_tool_contract::unknown_task_field_message(
            "create",
            "new_status",
            astra_tools::task_tool_contract::task_action_allowed_fields("create").unwrap(),
        );
        let cat = classify_error(&err);
        assert_eq!(cat, ErrorCategory::ToolInvalidArgs);
        let msg = build_recovery_message("task_board", &err, cat, &[]);
        assert!(
            msg.contains("action=update with task_id + new_status"),
            "wrong-action field errors must recover through the task contract: {msg}"
        );

        // write_file missing content → editing alternatives
        let err = "Error: Missing 'content' parameter. Retry write_file. Do not switch to bash.";
        let cat = classify_error(err);
        let msg = build_recovery_message("write_file", err, cat, &[]);
        assert!(
            msg.contains("str_replace") || msg.contains("multi_edit"),
            "editing alternatives: {msg}"
        );

        // ask_user invalid args → force retry
        let err =
            "Error: ask_user input is invalid. ask_user requires top-level 'questions': [...].";
        let cat = classify_error(err);
        let msg = build_recovery_message("ask_user", err, cat, &[]);
        assert!(msg.contains("Retry the SAME ask_user tool immediately"));
        assert!(msg.contains("\"questions\""));
    }

    // ── Escalation (13 original tests merged into 2) ──

    #[test]
    fn escalation_levels() {
        // Normal: fresh session, few errors, low nudges
        assert_eq!(escalation_level(0, 0, 0), EscalationLevel::Normal);
        assert_eq!(escalation_level(1, 0, 0), EscalationLevel::Normal);
        assert_eq!(escalation_level(2, 0, 0), EscalationLevel::Normal);
        assert_eq!(escalation_level(0, 3, 0), EscalationLevel::Normal);
        assert_eq!(escalation_level(0, 7, 0), EscalationLevel::Normal);

        // Warning: 3+ nudges or 8+ errors
        assert_eq!(escalation_level(3, 0, 0), EscalationLevel::Warning);
        assert_eq!(escalation_level(4, 0, 0), EscalationLevel::Warning);
        assert_eq!(escalation_level(0, 8, 0), EscalationLevel::Warning);
        assert_eq!(escalation_level(0, 14, 0), EscalationLevel::Warning);
        // 8 errors + 1 health-avoidance tool → Warning
        assert_eq!(escalation_level(0, 8, 1), EscalationLevel::Warning);

        // Critical: 4+ nudges + 3+ errors
        assert_eq!(escalation_level(4, 3, 0), EscalationLevel::Critical);
        assert_eq!(escalation_level(5, 4, 0), EscalationLevel::Critical);
        // 12+ errors + 2+ health-avoidance tools → Critical
        assert_eq!(escalation_level(0, 12, 2), EscalationLevel::Critical);
        assert_eq!(escalation_level(0, 13, 3), EscalationLevel::Critical);
        // 12 errors + 1 health-avoidance tool → Warning
        assert_eq!(escalation_level(0, 12, 1), EscalationLevel::Warning);
        // 15+ errors regardless → Critical
        assert_eq!(escalation_level(0, 15, 0), EscalationLevel::Critical);
        assert_eq!(escalation_level(0, 18, 0), EscalationLevel::Critical);
    }

    #[test]
    fn escalation_messages() {
        assert!(build_escalation_message(EscalationLevel::Normal, &[]).is_none());
        let msg = build_escalation_message(EscalationLevel::Warning, &["bash".to_string()]);
        assert!(msg.is_some());
        assert!(msg.unwrap().contains("bash"));
        let msg = build_escalation_message(EscalationLevel::Critical, &[]);
        assert!(msg.is_some());
        assert!(msg.unwrap().contains("MUST"));
    }

    // ── Session error summary (3 original tests merged into 2) ──

    #[test]
    fn error_summary_tracks_and_decays() {
        let mut summary = SessionErrorSummary::new();
        summary.record_error(ErrorCategory::Network);
        summary.record_error(ErrorCategory::Network);
        summary.record_error(ErrorCategory::Auth);
        assert_eq!(summary.total_errors, 3);
        assert_eq!(summary.errors_by_category[&ErrorCategory::Network], 2);
        assert_eq!(summary.errors_by_category[&ErrorCategory::Auth], 1);
        assert_eq!(summary.recent_error_pressure(), 3);

        summary.record_success();
        assert_eq!(summary.total_errors, 3);
        assert_eq!(summary.recent_error_pressure(), 2);

        summary.record_success();
        assert_eq!(summary.recent_error_pressure(), 1);

        summary.record_success();
        assert_eq!(summary.total_errors, 3);
        assert_eq!(summary.recent_error_pressure(), 0);
    }

    #[test]
    fn error_summary_caps_and_retry_rate() {
        let mut summary = SessionErrorSummary::new();
        for _ in 0..17 {
            summary.record_error(ErrorCategory::Network);
        }
        assert_eq!(summary.total_errors, 17);
        assert_eq!(summary.recent_error_pressure(), 16);

        // retry rate
        let mut summary = SessionErrorSummary::new();
        assert_eq!(summary.retry_success_rate(), 1.0);
        summary.record_error(ErrorCategory::Network);
        summary.record_error(ErrorCategory::Network);
        summary.record_retry(true);
        summary.record_retry(false);
        summary.record_retry(true);
        assert!((summary.retry_success_rate() - 0.6667).abs() < 0.01);
    }

    // ── Jitter ──

    #[test]
    fn retry_jitter_is_non_deterministic() {
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
