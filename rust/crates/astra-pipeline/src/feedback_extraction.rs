//! LLM-assisted structured feedback extraction.
//!
//! When a correction signal is detected (via regex in `implicit_feedback`),
//! this module builds a prompt for the LLM to extract structured feedback:
//! the rule, the reason (why), and when it applies.
//!
//! This bridges the gap between Astra's regex-based signal detection and
//! LLM-driven semantic understanding of user corrections.

use astra_turn_types::StructuredFeedback;

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

/// Directive keywords that indicate an actionable rule.
const DIRECTIVE_PREFIXES: &[&str] = &[
    "don't ",
    "do not ",
    "stop ",
    "never ",
    "should be ",
    "always ",
    "use ",
    "不要",
    "别",
    "应该",
];

/// Heuristic extraction — extracts structured feedback without LLM when the
/// correction is simple enough (e.g., "don't use X", "use Y instead").
///
/// Scans for directive keywords after common correction prefixes like
/// "wrong, ", "no, ", "that's incorrect, " etc. Extracts the directive
/// portion as the rule, not the full message.
///
/// Returns `None` if the correction is too complex for heuristic extraction.
pub fn heuristic_extract(
    correction_text: &str,
    source_signal: &str,
    confidence: f64,
) -> Option<StructuredFeedback> {
    let trimmed = correction_text.trim();
    if trimmed.is_empty() {
        return None;
    }

    let (rule, _offset) = extract_directive(trimmed)?;

    Some(StructuredFeedback {
        rule,
        reason: "Not stated".to_string(),
        apply_when: "General".to_string(),
        source_signal: source_signal.to_string(),
        confidence,
    })
}

/// Find a directive keyword in the text. Returns the directive portion
/// (from the keyword to end of text) and its byte offset.
///
/// Checks the start of the text first, then scans after punctuation
/// boundaries (", ", ". ", "，", "。") to handle "wrong, don't use X".
///
/// INVARIANT: byte offsets from `lower` are used to slice `text`. This is
/// correct only when `to_lowercase()` preserves byte lengths for all
/// characters in `DIRECTIVE_PREFIXES` and the separators. All current
/// entries (ASCII + CJK) satisfy this. If adding prefixes with
/// non-byte-stable lowercase mappings (e.g. German ß, Turkish İ),
/// switch to char-based indexing.
fn extract_directive(text: &str) -> Option<(String, usize)> {
    let lower = text.to_lowercase();

    // Check if text starts with a directive
    for prefix in DIRECTIVE_PREFIXES {
        if lower.starts_with(prefix) {
            return Some((text.to_string(), 0));
        }
    }

    // Scan after punctuation boundaries
    for sep in &[", ", ". ", "，", "。", "; "] {
        for (i, _) in lower.match_indices(sep) {
            let after = i + sep.len();
            let rest_lower = &lower[after..];
            for prefix in DIRECTIVE_PREFIXES {
                if rest_lower.starts_with(prefix) {
                    // Return the original-case text from the directive onward
                    return Some((text[after..].trim().to_string(), after));
                }
            }
        }
    }

    None
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
        let raw = "```json\n{\"rule\": \"No mocks\", \"reason\": \"Past incident\", \"apply_when\": \"Tests\"}\n```";
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
        assert_eq!(fb.rule, "do not run tests in parallel");
    }

    #[test]
    fn heuristic_stop_pattern() {
        let fb = heuristic_extract("stop summarizing at the end", "frustration", 0.7).unwrap();
        assert_eq!(fb.rule, "stop summarizing at the end");
    }

    #[test]
    fn heuristic_never_pattern() {
        let fb = heuristic_extract("never use force push", "correction", 0.8).unwrap();
        assert_eq!(fb.rule, "never use force push");
    }

    #[test]
    fn heuristic_always_pattern() {
        let fb = heuristic_extract("always run clippy before commit", "correction", 0.8).unwrap();
        assert_eq!(fb.rule, "always run clippy before commit");
    }

    #[test]
    fn heuristic_use_pattern() {
        let fb = heuristic_extract("use moerr instead of fmt.Errorf", "correction", 0.8).unwrap();
        assert_eq!(fb.rule, "use moerr instead of fmt.Errorf");
    }

    #[test]
    fn heuristic_chinese_bu_yao() {
        let fb = heuristic_extract("不要用bash执行git命令", "correction", 0.8).unwrap();
        assert_eq!(fb.rule, "不要用bash执行git命令");
    }

    #[test]
    fn heuristic_chinese_bie() {
        let fb = heuristic_extract("别再用这个方法了", "correction", 0.8).unwrap();
        assert_eq!(fb.rule, "别再用这个方法了");
    }

    #[test]
    fn heuristic_should_be_pattern() {
        let fb = heuristic_extract("should be using cargo test", "correction", 0.7).unwrap();
        assert_eq!(fb.rule, "should be using cargo test");
    }

    #[test]
    fn heuristic_chinese_ying_gai() {
        let fb = heuristic_extract("应该用moerr而不是fmt.Errorf", "correction", 0.8).unwrap();
        assert_eq!(fb.rule, "应该用moerr而不是fmt.Errorf");
    }

    // ── P0-1: directive after correction prefix ──

    #[test]
    fn heuristic_wrong_comma_dont() {
        // Real-world pattern: "wrong, don't use mocks"
        let fb = heuristic_extract("wrong, don't use mocks", "correction", 0.9).unwrap();
        assert_eq!(fb.rule, "don't use mocks");
    }

    #[test]
    fn heuristic_no_comma_never() {
        let fb = heuristic_extract("no, never force push on main", "correction", 0.8).unwrap();
        assert_eq!(fb.rule, "never force push on main");
    }

    #[test]
    fn heuristic_thats_incorrect_stop() {
        let fb =
            heuristic_extract("that's incorrect, stop using SELECT *", "correction", 0.8).unwrap();
        assert_eq!(fb.rule, "stop using SELECT *");
    }

    #[test]
    fn heuristic_chinese_prefix_bu_dui() {
        let fb = heuristic_extract("不对，不要用bash执行git命令", "correction", 0.8).unwrap();
        assert_eq!(fb.rule, "不要用bash执行git命令");
    }

    #[test]
    fn heuristic_period_separator() {
        let fb =
            heuristic_extract("That's wrong. Don't use mocks here.", "correction", 0.8).unwrap();
        assert_eq!(fb.rule, "Don't use mocks here.");
    }

    #[test]
    fn heuristic_semicolon_separator() {
        let fb = heuristic_extract("incorrect; always run tests first", "correction", 0.8).unwrap();
        assert_eq!(fb.rule, "always run tests first");
    }

    #[test]
    fn heuristic_preserves_original_case() {
        let fb = heuristic_extract("wrong, Don't Use Mocks", "correction", 0.9).unwrap();
        assert_eq!(fb.rule, "Don't Use Mocks");
    }

    // ── Negative cases ──

    #[test]
    fn heuristic_complex_returns_none() {
        assert!(
            heuristic_extract(
                "the approach you took doesn't work well for this codebase",
                "correction",
                0.7,
            )
            .is_none()
        );
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
        assert!(
            heuristic_extract(
                "I want to understand the code instead of just running it",
                "correction",
                0.7,
            )
            .is_none()
        );
    }

    #[test]
    fn heuristic_no_directive_after_prefix() {
        // "wrong, the approach is bad" — no directive keyword after comma
        assert!(heuristic_extract("wrong, the approach is bad", "correction", 0.7,).is_none());
    }

    // ── extract_directive unit tests ──

    #[test]
    fn extract_directive_at_start() {
        let (rule, offset) = extract_directive("don't use mocks").unwrap();
        assert_eq!(rule, "don't use mocks");
        assert_eq!(offset, 0);
    }

    #[test]
    fn extract_directive_after_comma() {
        let (rule, _) = extract_directive("wrong, don't use mocks").unwrap();
        assert_eq!(rule, "don't use mocks");
    }

    #[test]
    fn extract_directive_none_for_complex() {
        assert!(extract_directive("the approach doesn't work").is_none());
    }
}
