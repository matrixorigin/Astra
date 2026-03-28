//! Error classification, retry policy, and alternative tool suggestion.
//!
//! Provides a systematic approach to non-happy-path handling:
//! 1. **Error classification** — categorize tool errors into actionable types
//! 2. **Retry policy** — transient errors get automatic retry with backoff
//! 3. **Alternative suggestion** — when a tool fails, suggest domain alternatives
//! 4. **Progressive escalation** — each stall nudge gets stronger consequences

use std::collections::HashMap;

// ── Error Classification ─────────────────────────────────────────────────────

/// Categorized error type — determines retry and escalation strategy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorCategory {
    /// Network timeout, rate limit, temporary service unavailability.
    /// These should be retried with backoff.
    Transient,
    /// Auth failure, permission denied. Don't retry — suggest auth fix.
    Auth,
    /// Resource not found (file, repo, branch). Don't retry — suggest correction.
    NotFound,
    /// Invalid arguments (bad JSON, missing required param). Don't retry.
    InvalidArgs,
    /// Tool explicitly says it's not available or unsupported.
    Unavailable,
    /// System resource exhaustion (fork limit, OOM, disk full).
    /// Don't retry — block the tool immediately.
    ResourceLimit,
    /// Unknown error type — treat as permanent.
    Unknown,
}

/// Classify a tool error string into an actionable category.
pub fn classify_error(error_str: &str) -> ErrorCategory {
    let lower = error_str.to_lowercase();

    // Resource limit: fork exhaustion, OOM, disk full — never retry
    if lower.contains("resource temporarily unavailable")
        || lower.contains("cannot allocate memory")
        || lower.contains("out of memory")
        || lower.contains("no space left on device")
        || lower.contains("too many open files")
        || lower.contains("fork:")
        || lower.contains("enomem")
        || lower.contains("enospc")
        || lower.contains("ebusy")
        || lower.contains("device or resource busy")
        || lower.contains("资源暂时不足")
        || lower.contains("系统资源")
        || lower.contains("内存不足")
    {
        return ErrorCategory::ResourceLimit;
    }

    // Transient: network, timeout, rate limit, server errors
    if lower.contains("timeout")
        || lower.contains("timed out")
        || lower.contains("rate limit")
        || lower.contains("429")
        || lower.contains("500")
        || lower.contains("502")
        || lower.contains("503")
        || lower.contains("504")
        || lower.contains("internal server error")
        || lower.contains("service unavailable")
        || lower.contains("bad gateway")
        || lower.contains("gateway timeout")
        || lower.contains("connection refused")
        || lower.contains("connection reset")
        || lower.contains("network")
        || lower.contains("temporary")
        || lower.contains("retry")
        || lower.contains("econnrefused")
        || lower.contains("econnreset")
        || lower.contains("etimeout")
    {
        return ErrorCategory::Transient;
    }

    // Auth: permission, unauthorized, forbidden
    if lower.contains("unauthorized")
        || lower.contains("401")
        || lower.contains("403")
        || lower.contains("forbidden")
        || lower.contains("permission denied")
        || lower.contains("access denied")
        || lower.contains("authentication")
        || lower.contains("auth failed")
        || lower.contains("token expired")
        || lower.contains("invalid token")
        || lower.contains("could not validate")
        || lower.contains("credentials")
        || lower.contains("eacces")
        || lower.contains("eperm")
        || lower.contains("operation not permitted")
    {
        return ErrorCategory::Auth;
    }

    // Unavailable: not installed, not configured (check BEFORE NotFound
    // because "command not found" would otherwise match "not found")
    if lower.contains("not installed")
        || lower.contains("not configured")
        || lower.contains("unavailable")
        || lower.contains("not supported")
        || lower.contains("command not found")
    {
        return ErrorCategory::Unavailable;
    }

    // Not found: 404, no such file, repo not found
    if lower.contains("not found")
        || lower.contains("404")
        || lower.contains("no such file")
        || lower.contains("does not exist")
        || lower.contains("couldn't find")
        || lower.contains("unknown tool")
        || lower.contains("is a directory")
        || lower.contains("eisdir")
    {
        return ErrorCategory::NotFound;
    }

    // Invalid args: parse error, missing field, type mismatch
    if lower.contains("invalid")
        || lower.contains("parse error")
        || lower.contains("missing")
        || lower.contains("expected")
        || lower.contains("required field")
        || lower.contains("type mismatch")
        || lower.contains("malformed")
    {
        return ErrorCategory::InvalidArgs;
    }

    ErrorCategory::Unknown
}

// ── Retry Policy ─────────────────────────────────────────────────────────────

/// Maximum retries for transient errors on a single tool call.
/// Override with `MO_MAX_TOOL_RETRIES` env var.
pub fn max_tool_retries() -> usize {
    mo_agent_core::RuntimeLimits::global().max_tool_retries
}
/// Keep the constant for backward compatibility in tests.
pub const MAX_TOOL_RETRIES: usize = 2;

/// Base delay for retry backoff (milliseconds).
/// Override with `MO_RETRY_BASE_MS` env var.
pub fn retry_base_ms() -> u64 {
    mo_agent_core::RuntimeLimits::global().retry_base_ms
}
/// Keep the constant for backward compatibility in tests.
pub const RETRY_BASE_MS: u64 = 500;

/// Determine if and how to retry a failed tool call.
pub fn should_retry(category: ErrorCategory, attempt: usize) -> Option<u64> {
    if attempt >= max_tool_retries() {
        return None;
    }
    match category {
        ErrorCategory::Transient => {
            // Exponential backoff: 500ms, 1000ms, …
            Some(retry_base_ms() * (1 << attempt))
        }
        // All other categories: don't retry
        _ => None,
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
    &["write_file", "str_replace"],
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
    _error_str: &str,
    category: ErrorCategory,
    deprioritized: &[&str],
) -> String {
    let alternatives = suggest_alternatives(tool_name, deprioritized);

    let mut msg = match category {
        ErrorCategory::Transient => format!(
            "⚠ {} failed with a transient error (network/timeout). \
             The system retried automatically but it still failed.",
            tool_name
        ),
        ErrorCategory::Auth => format!(
            "⚠ {} failed with an authentication/permission error. \
             Do NOT retry — check credentials or use a different approach.",
            tool_name
        ),
        ErrorCategory::NotFound => format!(
            "⚠ {} failed: resource not found. \
             Verify the path/name is correct before retrying.",
            tool_name
        ),
        ErrorCategory::InvalidArgs => format!(
            "⚠ {} failed: invalid arguments. \
             Check the tool's expected parameters and fix the call.",
            tool_name
        ),
        ErrorCategory::Unavailable => format!(
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
        ErrorCategory::Unknown => format!("⚠ {} failed with an unclassified error.", tool_name),
    };

    if !alternatives.is_empty() {
        msg.push_str(&format!(" Alternatives: [{}].", alternatives.join(", ")));
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
    // Critical: 3+ nudges (lowered from 5: session 62c1e8e9 showed 11 stalls
    // without force_stop because the old threshold was too lenient),
    // or 8+ errors with at least one deprioritized tool,
    // or 10+ total errors regardless (scattered failures are still broken)
    if nudge_count >= 3
        || (total_errors >= 8 && deprioritized_count >= 1)
        || total_errors >= 10
    {
        return EscalationLevel::Critical;
    }
    // Warning: 2 nudges, or 5+ errors
    if nudge_count >= 2 || total_errors >= 5 {
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

// Allow ErrorCategory to be used as HashMap key
impl std::hash::Hash for ErrorCategory {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        core::mem::discriminant(self).hash(state);
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── Error classification ──

    #[test]
    fn classify_timeout() {
        assert_eq!(
            classify_error("connection timed out after 30s"),
            ErrorCategory::Transient
        );
        assert_eq!(classify_error("ETIMEOUT"), ErrorCategory::Transient);
    }

    #[test]
    fn classify_rate_limit() {
        assert_eq!(
            classify_error("rate limit exceeded (429)"),
            ErrorCategory::Transient
        );
        assert_eq!(
            classify_error("HTTP 503 Service Unavailable"),
            ErrorCategory::Transient
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
        assert_eq!(classify_error("404 Not Found"), ErrorCategory::NotFound);
        assert_eq!(
            classify_error("No such file or directory"),
            ErrorCategory::NotFound
        );
        assert_eq!(
            classify_error("Repository does not exist"),
            ErrorCategory::NotFound
        );
    }

    #[test]
    fn classify_invalid_args() {
        assert_eq!(
            classify_error("invalid JSON in arguments"),
            ErrorCategory::InvalidArgs
        );
        assert_eq!(
            classify_error("missing required field 'path'"),
            ErrorCategory::InvalidArgs
        );
    }

    #[test]
    fn classify_unavailable() {
        assert_eq!(
            classify_error("mysql: command not found"),
            ErrorCategory::Unavailable
        );
        assert_eq!(
            classify_error("Tool not configured for this environment"),
            ErrorCategory::Unavailable
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
            ErrorCategory::Transient
        );
        assert_eq!(
            classify_error("internal server error"),
            ErrorCategory::Transient
        );
    }

    #[test]
    fn classify_http_status_aliases_as_transient() {
        assert_eq!(classify_error("502 Bad Gateway"), ErrorCategory::Transient);
        assert_eq!(
            classify_error("service unavailable"),
            ErrorCategory::Transient
        );
        assert_eq!(classify_error("gateway timeout"), ErrorCategory::Transient);
    }

    // ── Retry policy ──

    #[test]
    fn retry_transient_first_attempt() {
        assert_eq!(should_retry(ErrorCategory::Transient, 0), Some(500));
    }

    #[test]
    fn retry_transient_second_attempt() {
        assert_eq!(should_retry(ErrorCategory::Transient, 1), Some(1000));
    }

    #[test]
    fn retry_transient_exhausted() {
        assert_eq!(should_retry(ErrorCategory::Transient, 2), None);
    }

    #[test]
    fn no_retry_auth() {
        assert_eq!(should_retry(ErrorCategory::Auth, 0), None);
    }

    #[test]
    fn no_retry_not_found() {
        assert_eq!(should_retry(ErrorCategory::NotFound, 0), None);
    }

    #[test]
    fn no_retry_invalid_args() {
        assert_eq!(should_retry(ErrorCategory::InvalidArgs, 0), None);
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

    // ── Recovery message ──

    #[test]
    fn recovery_message_includes_alternatives() {
        let msg = build_recovery_message("git_log", "timeout", ErrorCategory::Transient, &[]);
        assert!(msg.contains("git_diff"));
        assert!(msg.contains("Alternatives"));
    }

    #[test]
    fn recovery_message_auth_no_retry() {
        let msg = build_recovery_message("github_list_prs", "401", ErrorCategory::Auth, &[]);
        assert!(msg.contains("authentication"));
        assert!(msg.contains("Do NOT retry"));
    }

    #[test]
    fn recovery_message_unavailable() {
        let msg =
            build_recovery_message("mo_query", "not installed", ErrorCategory::Unavailable, &[]);
        assert!(msg.contains("not available"));
        assert!(msg.contains("alternative"));
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
    fn escalation_warning_two_nudges() {
        // 2 nudges → Warning (lowered from 3)
        assert_eq!(escalation_level(2, 0, 0), EscalationLevel::Warning);
    }

    #[test]
    fn escalation_critical_three_nudges() {
        // 3 nudges → Critical (lowered from 5: session 62c1e8e9 showed
        // 11 stalls without triggering Critical under old threshold)
        assert_eq!(escalation_level(3, 0, 0), EscalationLevel::Critical);
        assert_eq!(escalation_level(4, 0, 0), EscalationLevel::Critical);
    }

    #[test]
    fn escalation_warning_five_errors() {
        assert_eq!(escalation_level(0, 5, 0), EscalationLevel::Warning);
    }

    #[test]
    fn escalation_normal_few_errors() {
        // 3-4 errors: still Normal (raised from old threshold of 3)
        assert_eq!(escalation_level(0, 3, 0), EscalationLevel::Normal);
        assert_eq!(escalation_level(0, 4, 0), EscalationLevel::Normal);
    }

    #[test]
    fn escalation_critical_from_nudges() {
        assert_eq!(escalation_level(3, 0, 0), EscalationLevel::Critical);
        assert_eq!(escalation_level(5, 0, 0), EscalationLevel::Critical);
    }

    #[test]
    fn escalation_critical_many_errors_with_deprioritized() {
        // 8+ errors with at least 1 deprioritized tool → Critical (was 2, now 1)
        assert_eq!(escalation_level(0, 8, 1), EscalationLevel::Critical);
        assert_eq!(escalation_level(0, 8, 2), EscalationLevel::Critical);
    }

    #[test]
    fn escalation_not_critical_errors_without_deprioritized() {
        // 8 errors but no deprioritized tools → Warning, not Critical
        assert_eq!(escalation_level(0, 8, 0), EscalationLevel::Warning);
    }

    #[test]
    fn escalation_critical_ten_errors_regardless() {
        // 10+ errors with zero deprioritized → Critical (new: standalone high-error gate)
        assert_eq!(escalation_level(0, 10, 0), EscalationLevel::Critical);
        assert_eq!(escalation_level(0, 12, 0), EscalationLevel::Critical);
    }

    #[test]
    fn escalation_nine_errors_no_deprioritized_is_warning() {
        // Below the standalone threshold, no deprioritized → stays Warning
        assert_eq!(escalation_level(0, 9, 0), EscalationLevel::Warning);
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
        summary.record_error(ErrorCategory::Transient);
        summary.record_error(ErrorCategory::Transient);
        summary.record_error(ErrorCategory::Auth);
        assert_eq!(summary.total_errors, 3);
        assert_eq!(summary.errors_by_category[&ErrorCategory::Transient], 2);
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
        assert_ne!(cat, ErrorCategory::Transient);
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
            ErrorCategory::NotFound
        );
    }

    #[test]
    fn classify_is_a_directory_as_not_found() {
        assert_eq!(
            classify_error("Error: Is a directory"),
            ErrorCategory::NotFound
        );
    }
}
