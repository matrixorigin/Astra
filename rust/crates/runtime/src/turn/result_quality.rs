//! Tool result quality classification.
//!
//! Classifies tool results beyond binary success/error into richer categories
//! that drive smarter retry, escalation, and feedback decisions.

/// Quality classification for a tool result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResultQuality {
    /// Tool returned meaningful data.
    Success,
    /// Tool returned an explicit error message.
    Error,
    /// Tool returned empty or effectively-empty data (`{}`, `[]`, `""`, `null`).
    /// Not an error per se, but the LLM may retry fruitlessly.
    Empty,
    /// Tool returned data that looks truncated (e.g., too large, cut off).
    Truncated,
}

/// Classify a tool result string into a quality category.
///
/// This is a general-purpose classifier — it works with any tool's output,
/// not specific to individual tools.
pub fn classify_result(result_str: &str) -> ResultQuality {
    let trimmed = result_str.trim();

    // Empty string
    if trimmed.is_empty() {
        return ResultQuality::Empty;
    }

    // Try to parse as JSON
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(trimmed) {
        // Explicit error field
        if let Some(err) = value.get("error")
            && !err.is_null()
            && err.as_str() != Some("")
        {
            return ResultQuality::Error;
        }
        // ok: false
        if let Some(ok_val) = value.get("ok").and_then(|o| o.as_bool())
            && !ok_val
        {
            return ResultQuality::Error;
        }
        // Empty JSON structures
        if value.is_null() {
            return ResultQuality::Empty;
        }
        if let Some(obj) = value.as_object()
            && obj.is_empty()
        {
            return ResultQuality::Empty;
        }
        if let Some(arr) = value.as_array()
            && arr.is_empty()
        {
            return ResultQuality::Empty;
        }
        if value.as_str() == Some("") {
            return ResultQuality::Empty;
        }
        // Truncation markers
        if let Some(s) = value.as_str()
            && (s.ends_with("...") || s.ends_with("[truncated]") || s.contains("output truncated"))
        {
            return ResultQuality::Truncated;
        }
        return ResultQuality::Success;
    }

    // Non-JSON: check for error prefixes
    let lower = trimmed.to_lowercase();
    if lower.starts_with("error")
        || lower.starts_with("failed")
        || lower.starts_with("fatal")
        || lower.starts_with("exception")
    {
        return ResultQuality::Error;
    }

    // Truncation indicators in plain text
    if trimmed.ends_with("...") && trimmed.len() > 500 {
        return ResultQuality::Truncated;
    }
    if lower.contains("[truncated]") || lower.contains("output truncated") {
        return ResultQuality::Truncated;
    }

    ResultQuality::Success
}

/// Build a feedback message for the LLM based on result quality.
/// Returns None for Success (no feedback needed).
pub fn quality_feedback(tool_name: &str, quality: ResultQuality) -> Option<String> {
    match quality {
        ResultQuality::Success => None,
        ResultQuality::Error => Some(format!(
            "⚠ {} returned an error. Check the error details and either fix the arguments or try an alternative tool.",
            tool_name
        )),
        ResultQuality::Empty => Some(format!(
            "ℹ {} returned empty results. The query may need different parameters, \
             or the resource may not exist. Do NOT retry with the same arguments.",
            tool_name
        )),
        ResultQuality::Truncated => Some(format!(
            "ℹ {} results were truncated. Consider narrowing the query or using pagination.",
            tool_name
        )),
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── Empty detection ──

    #[test]
    fn empty_string() {
        assert_eq!(classify_result(""), ResultQuality::Empty);
    }

    #[test]
    fn whitespace_only() {
        assert_eq!(classify_result("   \n  "), ResultQuality::Empty);
    }

    #[test]
    fn empty_json_object() {
        assert_eq!(classify_result("{}"), ResultQuality::Empty);
    }

    #[test]
    fn empty_json_array() {
        assert_eq!(classify_result("[]"), ResultQuality::Empty);
    }

    #[test]
    fn null_json() {
        assert_eq!(classify_result("null"), ResultQuality::Empty);
    }

    #[test]
    fn empty_string_json() {
        assert_eq!(classify_result(r#""""#), ResultQuality::Empty);
    }

    // ── Error detection ──

    #[test]
    fn json_error_field() {
        assert_eq!(
            classify_result(r#"{"error": "file not found"}"#),
            ResultQuality::Error
        );
    }

    #[test]
    fn json_ok_false() {
        assert_eq!(
            classify_result(r#"{"ok": false, "message": "timeout"}"#),
            ResultQuality::Error
        );
    }

    #[test]
    fn plain_text_error_prefix() {
        assert_eq!(
            classify_result("Error: command not found"),
            ResultQuality::Error
        );
    }

    #[test]
    fn plain_text_fatal() {
        assert_eq!(
            classify_result("Fatal: repository not found"),
            ResultQuality::Error
        );
    }

    #[test]
    fn null_error_field_is_not_error() {
        assert_eq!(
            classify_result(r#"{"error": null, "data": "ok"}"#),
            ResultQuality::Success
        );
    }

    #[test]
    fn empty_error_string_is_not_error() {
        assert_eq!(
            classify_result(r#"{"error": "", "data": "ok"}"#),
            ResultQuality::Success
        );
    }

    // ── Truncation detection ──

    #[test]
    fn json_string_truncated_marker() {
        assert_eq!(
            classify_result(r#""very long output...[truncated]""#),
            ResultQuality::Truncated
        );
    }

    #[test]
    fn plain_text_truncation() {
        let long = "x".repeat(501) + "...";
        assert_eq!(classify_result(&long), ResultQuality::Truncated);
    }

    // ── Success ──

    #[test]
    fn normal_json_object() {
        assert_eq!(
            classify_result(r#"{"status": "ok", "count": 42}"#),
            ResultQuality::Success
        );
    }

    #[test]
    fn normal_json_array() {
        assert_eq!(
            classify_result(r#"[{"id": 1}, {"id": 2}]"#),
            ResultQuality::Success
        );
    }

    #[test]
    fn plain_text_success() {
        assert_eq!(
            classify_result("commit abc123: fix bug in parser"),
            ResultQuality::Success
        );
    }

    #[test]
    fn short_ellipsis_not_truncated() {
        // Short strings ending in "..." are not truncation
        assert_eq!(classify_result("loading..."), ResultQuality::Success);
    }

    // ── Feedback messages ──

    #[test]
    fn success_no_feedback() {
        assert!(quality_feedback("bash", ResultQuality::Success).is_none());
    }

    #[test]
    fn error_feedback_has_tool_name() {
        let msg = quality_feedback("github_list_prs", ResultQuality::Error).unwrap();
        assert!(msg.contains("github_list_prs"));
        assert!(msg.contains("error"));
    }

    #[test]
    fn empty_feedback_warns_no_retry() {
        let msg = quality_feedback("grep", ResultQuality::Empty).unwrap();
        assert!(msg.contains("grep"));
        assert!(msg.contains("Do NOT retry"));
    }

    #[test]
    fn truncated_feedback_suggests_narrow() {
        let msg = quality_feedback("git_log", ResultQuality::Truncated).unwrap();
        assert!(msg.contains("truncated"));
    }
}
