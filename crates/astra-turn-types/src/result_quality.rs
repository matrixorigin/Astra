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
        if let Some(status) = value.get("status").and_then(|s| s.as_str()) {
            let status = status.trim().to_ascii_lowercase();
            if matches!(
                status.as_str(),
                "failed" | "error" | "cancelled" | "canceled" | "aborted" | "timeout" | "timed_out"
            ) {
                return ResultQuality::Error;
            }
            if matches!(
                status.as_str(),
                "pending"
                    | "queued"
                    | "in_progress"
                    | "running"
                    | "still_running"
                    | "processing"
                    | "starting"
            ) && value.get("result").is_none()
                && value.get("output").is_none()
                && value.get("data").is_none()
            {
                return ResultQuality::Empty;
            }
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
            "ℹ {} returned no finished result. The query may need different parameters, \
             the resource may not exist, or the work may not be ready yet. Do NOT retry \
             with the same arguments.",
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

    #[test]
    fn empty_classifications() {
        let cases: &[&str] = &[
            "",
            "   \n  ",
            "{}",
            "[]",
            "null",
            "\"\"",
            "{\"status\":\"still_running\",\"agent_id\":\"agent-123\"}",
        ];
        for input in cases {
            assert_eq!(
                classify_result(input),
                ResultQuality::Empty,
                "input: {input:?}"
            );
        }
    }

    #[test]
    fn error_classifications() {
        let cases: &[&str] = &[
            "{\"error\": \"file not found\"}",
            "{\"ok\": false, \"message\": \"timeout\"}",
            "{\"status\":\"failed\",\"agent_id\":\"agent-123\"}",
            "{\"status\":\"cancelled\",\"agent_id\":\"agent-123\"}",
            "Error: command not found",
            "Fatal: repository not found",
        ];
        for input in cases {
            assert_eq!(
                classify_result(input),
                ResultQuality::Error,
                "input: {input:?}"
            );
        }
    }

    #[test]
    fn null_or_empty_error_field_is_not_error() {
        assert_eq!(
            classify_result("{\"error\": null, \"data\": \"ok\"}"),
            ResultQuality::Success
        );
        assert_eq!(
            classify_result("{\"error\": \"\", \"data\": \"ok\"}"),
            ResultQuality::Success
        );
    }

    #[test]
    fn truncation_classifications() {
        assert_eq!(
            classify_result("\"very long output...[truncated]\""),
            ResultQuality::Truncated
        );
        let long = "x".repeat(501) + "...";
        assert_eq!(classify_result(&long), ResultQuality::Truncated);
    }

    #[test]
    fn success_classifications() {
        let cases: &[&str] = &[
            "{\"status\": \"ok\", \"count\": 42}",
            "{\"status\":\"completed\",\"agent_id\":\"agent-123\"}",
            "[{\"id\": 1}, {\"id\": 2}]",
            "commit abc123: fix bug in parser",
        ];
        for input in cases {
            assert_eq!(
                classify_result(input),
                ResultQuality::Success,
                "input: {input:?}"
            );
        }
    }

    #[test]
    fn short_ellipsis_not_truncated() {
        assert_eq!(classify_result("loading..."), ResultQuality::Success);
    }

    #[test]
    fn quality_feedback_messages() {
        let cases: &[(&str, ResultQuality, Option<&str>)] = &[
            ("bash", ResultQuality::Success, None),
            ("github", ResultQuality::Error, Some("error")),
            ("grep", ResultQuality::Empty, Some("Do NOT retry")),
            ("git", ResultQuality::Truncated, Some("truncated")),
        ];
        for (tool_name, quality, expected_substr) in cases {
            let msg = quality_feedback(tool_name, *quality);
            match expected_substr {
                Some(substr) => assert!(
                    msg.unwrap().contains(substr),
                    "tool={tool_name} quality={quality:?}"
                ),
                None => assert!(msg.is_none(), "tool={tool_name} quality={quality:?}"),
            }
        }
    }
}
