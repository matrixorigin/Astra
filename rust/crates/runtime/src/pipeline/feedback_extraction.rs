//! LLM-assisted structured feedback extraction.
//!
//! When a correction signal is detected (via regex in `implicit_feedback`),
//! this module builds a prompt for the LLM to extract structured feedback:
//! the rule, the reason (why), and when it applies.
//!
//! This bridges the gap between Astra's regex-based signal detection and
//! Claude Code's LLM-driven semantic understanding of user corrections.

use astra_turn_types::StructuredFeedback;

/// System prompt for the feedback extraction sub-task.
pub const FEEDBACK_EXTRACTION_SYSTEM: &str = "\
You are a feedback extraction agent. Given a user correction and the prior assistant response, \
extract a structured feedback rule. Respond with ONLY a JSON object, no other text.

Format:
{
  \"rule\": \"What the user wants changed (the actionable directive)\",
  \"reason\": \"Why — the incident, preference, or past failure that motivated this\",
  \"apply_when\": \"When/where this rule applies (domain, task type, tool, or situation)\"
}

Guidelines:
- \"rule\" should be a clear, actionable directive (e.g., \"Use real database in integration tests, not mocks\")
- \"reason\" should capture the WHY — if the user didn't state one, write \"Not stated\"
- \"apply_when\" should be specific (e.g., \"When writing integration tests for the auth module\") not vague
- If the correction is too vague to extract a rule, set rule to the correction text verbatim
- Keep each field under 200 characters";

/// Build the user message for feedback extraction.
pub fn build_extraction_message(correction_text: &str, prior_assistant_text: &str) -> String {
    let prior = if prior_assistant_text.is_empty() {
        "(no prior response)".to_string()
    } else {
        truncate(prior_assistant_text, 500)
    };
    format!("Prior assistant response:\n{prior}\n\nUser correction:\n{correction_text}")
}

/// Parse the LLM's JSON response into a `StructuredFeedback`.
///
/// Returns `None` if the response is not valid JSON or missing required fields.
pub fn parse_extraction_response(
    raw: &str,
    source_signal: &str,
    confidence: f64,
) -> Option<StructuredFeedback> {
    let trimmed = raw.trim();

    // Strip code fences: ```json ... ``` or ``` ... ```
    let json_str = if let Some(rest) = trimmed.strip_prefix("```") {
        let body = rest.strip_prefix("json").unwrap_or(rest);
        body.trim().strip_suffix("```").unwrap_or(body).trim()
    } else {
        trimmed
    };

    let v: serde_json::Value = serde_json::from_str(json_str).ok()?;
    let obj = v.as_object()?;

    let rule = obj.get("rule")?.as_str()?.to_string();
    if rule.is_empty() {
        return None;
    }

    Some(StructuredFeedback {
        rule,
        reason: obj
            .get("reason")
            .and_then(|v| v.as_str())
            .unwrap_or("Not stated")
            .to_string(),
        apply_when: obj
            .get("apply_when")
            .and_then(|v| v.as_str())
            .unwrap_or("General")
            .to_string(),
        source_signal: source_signal.to_string(),
        confidence,
    })
}

/// Heuristic extraction — extracts structured feedback without LLM when the
/// correction is simple enough (e.g., "don't use X", "use Y instead").
///
/// Returns `None` if the correction is too complex for heuristic extraction.
pub fn heuristic_extract(
    correction_text: &str,
    source_signal: &str,
    confidence: f64,
) -> Option<StructuredFeedback> {
    let lower = correction_text.to_lowercase();
    let trimmed = lower.trim();

    if trimmed.is_empty() {
        return None;
    }

    // Only match when the directive keyword is at the start of the sentence
    let is_simple_directive = trimmed.starts_with("don't ")
        || trimmed.starts_with("do not ")
        || trimmed.starts_with("stop ")
        || trimmed.starts_with("never ")
        || trimmed.starts_with("不要")
        || trimmed.starts_with("别")
        || trimmed.starts_with("should be ")
        || trimmed.starts_with("应该");

    if !is_simple_directive {
        return None;
    }

    Some(StructuredFeedback {
        rule: correction_text.to_string(),
        reason: "Not stated".to_string(),
        apply_when: "General".to_string(),
        source_signal: source_signal.to_string(),
        confidence,
    })
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        let end = s.floor_char_boundary(max);
        s[..end].to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── parse_extraction_response ──

    #[test]
    fn parse_valid_json() {
        let raw = r#"{"rule": "Use real DB in tests", "reason": "Mocks diverged from prod", "apply_when": "Integration tests"}"#;
        let fb = parse_extraction_response(raw, "correction", 0.9).unwrap();
        assert_eq!(fb.rule, "Use real DB in tests");
        assert_eq!(fb.reason, "Mocks diverged from prod");
        assert_eq!(fb.apply_when, "Integration tests");
        assert_eq!(fb.source_signal, "correction");
        assert_eq!(fb.confidence, 0.9);
    }

    #[test]
    fn parse_json_with_code_fences() {
        let raw =
            "```json\n{\"rule\": \"No mocks\", \"reason\": \"Past incident\", \"apply_when\": \"Tests\"}\n```";
        let fb = parse_extraction_response(raw, "frustration", 0.7).unwrap();
        assert_eq!(fb.rule, "No mocks");
    }

    #[test]
    fn parse_json_with_bare_code_fences() {
        let raw = "```\n{\"rule\": \"Run clippy\"}\n```";
        let fb = parse_extraction_response(raw, "correction", 0.8).unwrap();
        assert_eq!(fb.rule, "Run clippy");
    }

    #[test]
    fn parse_json_with_trailing_newline_in_fence() {
        let raw = "```json\n{\"rule\": \"Check types\"}\n```\n";
        let fb = parse_extraction_response(raw, "correction", 0.8).unwrap();
        assert_eq!(fb.rule, "Check types");
    }

    #[test]
    fn parse_missing_reason_defaults() {
        let raw = r#"{"rule": "Always run clippy"}"#;
        let fb = parse_extraction_response(raw, "correction", 0.8).unwrap();
        assert_eq!(fb.reason, "Not stated");
        assert_eq!(fb.apply_when, "General");
    }

    #[test]
    fn parse_empty_rule_returns_none() {
        let raw = r#"{"rule": "", "reason": "x"}"#;
        assert!(parse_extraction_response(raw, "correction", 0.8).is_none());
    }

    #[test]
    fn parse_garbage_returns_none() {
        assert!(parse_extraction_response("not json", "correction", 0.8).is_none());
    }

    #[test]
    fn parse_empty_string_returns_none() {
        assert!(parse_extraction_response("", "correction", 0.8).is_none());
    }

    #[test]
    fn parse_json_array_returns_none() {
        let raw = r#"[{"rule": "x"}]"#;
        assert!(parse_extraction_response(raw, "correction", 0.8).is_none());
    }

    #[test]
    fn parse_rule_is_number_returns_none() {
        let raw = r#"{"rule": 42}"#;
        assert!(parse_extraction_response(raw, "correction", 0.8).is_none());
    }

    #[test]
    fn parse_rule_is_object_returns_none() {
        let raw = r#"{"rule": {"nested": true}}"#;
        assert!(parse_extraction_response(raw, "correction", 0.8).is_none());
    }

    // ── heuristic_extract ──

    #[test]
    fn heuristic_dont_pattern() {
        let fb = heuristic_extract("don't use mocks in tests", "correction", 0.9).unwrap();
        assert_eq!(fb.rule, "don't use mocks in tests");
        assert_eq!(fb.reason, "Not stated");
    }

    #[test]
    fn heuristic_do_not_pattern() {
        let fb = heuristic_extract("do not run tests in parallel", "correction", 0.8).unwrap();
        assert!(fb.rule.contains("do not"));
    }

    #[test]
    fn heuristic_stop_pattern() {
        let fb = heuristic_extract("stop summarizing at the end", "frustration", 0.7).unwrap();
        assert_eq!(fb.rule, "stop summarizing at the end");
    }

    #[test]
    fn heuristic_never_pattern() {
        let fb = heuristic_extract("never use force push", "correction", 0.8).unwrap();
        assert!(fb.rule.contains("never"));
    }

    #[test]
    fn heuristic_chinese_bu_yao() {
        let fb = heuristic_extract("不要用bash执行git命令", "correction", 0.8).unwrap();
        assert!(fb.rule.contains("不要"));
    }

    #[test]
    fn heuristic_chinese_bie() {
        let fb = heuristic_extract("别再用这个方法了", "correction", 0.8).unwrap();
        assert!(fb.rule.contains("别"));
    }

    #[test]
    fn heuristic_should_be_pattern() {
        let fb = heuristic_extract("should be using cargo test", "correction", 0.7).unwrap();
        assert!(fb.rule.contains("should be"));
    }

    #[test]
    fn heuristic_chinese_ying_gai() {
        let fb = heuristic_extract("应该用moerr而不是fmt.Errorf", "correction", 0.8).unwrap();
        assert!(fb.rule.contains("应该"));
    }

    #[test]
    fn heuristic_complex_returns_none() {
        assert!(heuristic_extract(
            "the approach you took doesn't work well for this codebase",
            "correction",
            0.7,
        )
        .is_none());
    }

    #[test]
    fn heuristic_empty_returns_none() {
        assert!(heuristic_extract("", "correction", 0.7).is_none());
    }

    #[test]
    fn heuristic_whitespace_returns_none() {
        assert!(heuristic_extract("   ", "correction", 0.7).is_none());
    }

    #[test]
    fn heuristic_instead_mid_sentence_not_matched() {
        // "instead" in the middle of a complex sentence should NOT match
        // (removed the overly broad "contains instead" check)
        assert!(heuristic_extract(
            "I want to understand the code instead of just running it",
            "correction",
            0.7,
        )
        .is_none());
    }

    // ── build_extraction_message ──

    #[test]
    fn build_message_with_prior() {
        let msg = build_extraction_message("wrong approach", "I used method A");
        assert!(msg.contains("I used method A"));
        assert!(msg.contains("wrong approach"));
    }

    #[test]
    fn build_message_without_prior() {
        let msg = build_extraction_message("wrong approach", "");
        assert!(msg.contains("(no prior response)"));
    }

    #[test]
    fn build_message_truncates_long_prior() {
        let long = "x".repeat(1000);
        let msg = build_extraction_message("fix it", &long);
        assert!(msg.len() < 1000);
    }

    #[test]
    fn system_prompt_is_well_formed() {
        assert!(FEEDBACK_EXTRACTION_SYSTEM.contains("rule"));
        assert!(FEEDBACK_EXTRACTION_SYSTEM.contains("reason"));
        assert!(FEEDBACK_EXTRACTION_SYSTEM.contains("apply_when"));
        assert!(FEEDBACK_EXTRACTION_SYSTEM.contains("JSON"));
    }
}
