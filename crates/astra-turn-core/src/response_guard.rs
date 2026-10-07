// ── Fallback messages when guards fire ──────────────────────────────
/// Replacement text when the LLM leaks the system prompt.
pub const PROMPT_LEAK_FALLBACK: &str =
    "I can’t provide internal system instructions, but I can still help with the visible request.";

/// Replacement text when an internal control protocol leaks into final output.
pub const INTERNAL_PROTOCOL_FALLBACK: &str = "I can’t use an internal control message as a user-facing answer, but I can still help with your visible request.";

/// Finish reason used when a safety fallback replaces a child agent's final
/// text. The original output is withheld, but the visible fallback is a
/// completed response: parent orchestration must not retry or abort solely
/// because this safety boundary was applied.
pub const RESPONSE_GUARD_REDACTED_FINISH_REASON: &str = "safety_redacted";

/// Apply hard guards and advisory quality checks to a final text response.
/// The caller owns the boundary that separates final text from tool preambles.
pub fn apply_response_guards(text: &str, user_query: &str) -> ResponseGuardResult {
    if text.is_empty() {
        return ResponseGuardResult {
            replacement: None,
            quality: QualityReport::default(),
        };
    }

    // Hard blocks: replace entire text
    if contains_internal_protocol_marker(text) {
        return ResponseGuardResult {
            replacement: Some(INTERNAL_PROTOCOL_FALLBACK.to_string()),
            quality: QualityReport::default(),
        };
    }
    if is_prompt_leaked(text, &[]) {
        return ResponseGuardResult {
            replacement: Some(PROMPT_LEAK_FALLBACK.to_string()),
            quality: QualityReport::default(),
        };
    }
    // Soft signals: return quality report (caller decides what to do)
    let mut quality = check_response_quality(text, user_query);
    quality.has_repetition_loop = is_repetition_loop(text);
    ResponseGuardResult {
        replacement: None,
        quality,
    }
}

/// Outcome of running all response guards on LLM output.
#[derive(Debug, Clone)]
pub struct ResponseGuardResult {
    /// If `Some`, the original text should be replaced with this fallback.
    pub replacement: Option<String>,
    /// Quality signals (fabrication, echo, hallucination) — advisory only.
    pub quality: QualityReport,
}

const STRUCTURAL_MARKERS: &[&str] = &[
    "## Core Rules",
    "## Planning Protocol",
    "## Self-Model",
    "## Conversation History",
    "File editing rules:",
    "Tool surface rules:",
    "Reflection rules:",
    "Introspection rules:",
];

const INTERNAL_PROTOCOL_MARKERS: &[&str] = &[
    "<ask_astra_data",
    "</ask_astra_data>",
    "<astra_internal",
    "</astra_internal>",
    "__astra_",
];

const REPEAT_THRESHOLD: usize = 8;

/// Patterns that indicate the LLM fabricated file paths or data.
/// These are common hallucination signatures — paths that look plausible but are invented.
/// Kept specific to avoid false-positiving on legitimate URLs.
const FABRICATION_MARKERS: &[&str] = &[
    "path/to/your/",
    "path/to/project/",
    "/example/project/",
    "<YOUR_",
    "<your_",
    "INSERT_YOUR_",
    "REPLACE_WITH_",
    "TODO_REPLACE",
];

pub fn is_prompt_leaked(text: &str, fingerprints: &[String]) -> bool {
    if text.is_empty() {
        return false;
    }

    let visible_text = text_without_code_regions(text);
    if STRUCTURAL_MARKERS
        .iter()
        .any(|marker| visible_text.contains(marker))
    {
        return true;
    }

    if fingerprints.is_empty() {
        return false;
    }

    let lower = visible_text.to_lowercase();
    fingerprints
        .iter()
        .any(|fingerprint| lower.contains(fingerprint))
}

pub fn contains_internal_protocol_marker(text: &str) -> bool {
    if text.is_empty() {
        return false;
    }

    let lower = text_without_code_regions(text).to_ascii_lowercase();
    INTERNAL_PROTOCOL_MARKERS
        .iter()
        .any(|marker| lower.contains(marker))
}

fn text_without_code_regions(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut in_fence = false;
    for line in text.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            in_fence = !in_fence;
            out.push('\n');
            continue;
        }
        if in_fence {
            out.push('\n');
            continue;
        }
        out.push_str(&line_without_inline_code(line));
        out.push('\n');
    }
    out
}

fn line_without_inline_code(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut in_inline_code = false;
    for ch in line.chars() {
        if ch == '`' {
            in_inline_code = !in_inline_code;
            out.push(' ');
        } else if !in_inline_code {
            out.push(ch);
        }
    }
    out
}

pub fn is_repetition_loop(text: &str) -> bool {
    if text.is_empty() {
        return false;
    }

    let words = text.split_whitespace().collect::<Vec<_>>();
    if words.len() < REPEAT_THRESHOLD {
        return false;
    }

    let mut count = 1usize;
    for pair in words.windows(2) {
        if pair[0].eq_ignore_ascii_case(pair[1]) {
            count += 1;
            if count >= REPEAT_THRESHOLD {
                return true;
            }
        } else {
            count = 1;
        }
    }
    false
}

// ── Response quality signals ────────────────────────────────────────

/// Quality issues detected in a response.
#[derive(Debug, Clone, Default)]
pub struct QualityReport {
    /// Whether the response contains fabricated path/data markers.
    pub has_fabrication_markers: bool,
    /// Whether the response is a non-answer (just the user's question echoed back).
    pub is_echo: bool,
    /// Whether the output contains a repeated-token loop. Advisory only.
    pub has_repetition_loop: bool,
}

/// Inspect advisory quality signals in a final text response.
///
/// * `text`       – the final text response
/// * `user_query`    – the user's original message (for echo detection)
pub fn check_response_quality(text: &str, user_query: &str) -> QualityReport {
    // Fabrication detection: check text response for placeholder patterns
    let has_fabrication_markers = if text.len() > 20 {
        FABRICATION_MARKERS
            .iter()
            .any(|marker| text.contains(marker))
    } else {
        false
    };

    // Echo detection: LLM just repeated user's question
    let is_echo = if !user_query.is_empty() && !text.is_empty() && user_query.len() > 10 {
        let query_trimmed = user_query.trim();
        let text_trimmed = text.trim();
        // Exact or near-exact echo (text is just the query with minor additions)
        text_trimmed == query_trimmed
            || (text_trimmed.len() < query_trimmed.len() * 2
                && text_trimmed.contains(query_trimmed))
    } else {
        false
    };

    QualityReport {
        has_fabrication_markers,
        is_echo,
        has_repetition_loop: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Existing tests ──────────────────────────────────────────

    #[test]
    fn prompt_leak_detected() {
        assert!(is_prompt_leaked("## Core Rules are important", &[]));
        assert!(is_prompt_leaked("## Planning Protocol details", &[]));
        assert!(is_prompt_leaked("here are File editing rules: ...", &[]));
    }

    #[test]
    fn prompt_leak_markers_inside_code_are_not_blocked() {
        let review = "This diff defines:\n```md\n## Core Rules\nTool surface rules:\n```";
        assert!(!is_prompt_leaked(review, &[]));
    }

    #[test]
    fn prompt_leak_with_fingerprints() {
        let fps = vec!["secret_key_abc".to_string()];
        assert!(is_prompt_leaked("contains secret_key_abc", &fps));
        assert!(!is_prompt_leaked("normal text", &fps));
    }

    #[test]
    fn prompt_leak_empty_text() {
        assert!(!is_prompt_leaked("", &[]));
    }

    #[test]
    fn repetition_loop_detected() {
        assert!(is_repetition_loop(
            "hello hello hello hello hello hello hello hello"
        ));
    }

    #[test]
    fn repetition_loop_not_triggered_normal() {
        assert!(!is_repetition_loop(
            "the quick brown fox jumps over the lazy dog"
        ));
    }

    #[test]
    fn repetition_loop_empty() {
        assert!(!is_repetition_loop(""));
    }

    #[test]
    fn repetition_loop_short() {
        assert!(!is_repetition_loop("hello hello hello"));
    }

    #[test]
    fn internal_protocol_markers_inside_code_are_not_blocked() {
        let review = "The code contains `__astra_required_runtime_context` and:\n```rust\nlet tag = \"<ask_astra_data>\";\n```";
        assert!(!contains_internal_protocol_marker(review));
        let result = apply_response_guards(review, "review guard code");
        assert!(result.replacement.is_none());
    }

    #[test]
    fn visible_internal_protocol_marker_is_still_blocked() {
        assert!(contains_internal_protocol_marker(
            "<ask_astra_data><query>secret</query></ask_astra_data>"
        ));
    }

    // ── Fabrication markers ─────────────────────────────────────

    #[test]
    fn fabrication_detected_in_text() {
        let report = check_response_quality(
            "You can find the config at path/to/your/config.yaml and edit it",
            "where is the config?",
        );
        assert!(report.has_fabrication_markers);
    }

    #[test]
    fn fabrication_not_triggered_for_real_paths() {
        let report = check_response_quality(
            "The config is at crates/runtime/src/config.rs",
            "where is the config?",
        );
        assert!(!report.has_fabrication_markers);
    }

    #[test]
    fn fabrication_not_triggered_for_legitimate_urls() {
        let report = check_response_quality(
            "See https://api.github.com/path/to/repo for the API docs and example.com/api/v1",
            "where are the docs?",
        );
        assert!(
            !report.has_fabrication_markers,
            "legitimate URLs should not trigger fabrication"
        );
    }

    #[test]
    fn fabrication_not_triggered_for_short_text() {
        let report = check_response_quality("Done.", "fix it");
        assert!(!report.has_fabrication_markers);
    }

    // ── Echo detection ──────────────────────────────────────────

    #[test]
    fn echo_detected() {
        let query = "How does authentication work in this project?";
        let report = check_response_quality(query, query);
        assert!(report.is_echo);
    }

    #[test]
    fn echo_not_triggered_with_real_answer() {
        let report = check_response_quality(
            "Authentication uses JWT tokens stored in cookies.",
            "How does authentication work?",
        );
        assert!(!report.is_echo);
    }

    // ── apply_response_guards ───────────────────────────────────

    #[test]
    fn guard_blocks_prompt_leak() {
        let result =
            apply_response_guards("Here are ## Core Rules that must be followed", "help me");
        assert_eq!(result.replacement.as_deref(), Some(PROMPT_LEAK_FALLBACK));
    }

    #[test]
    fn guard_reports_repetition_without_replacing_output() {
        let result =
            apply_response_guards("loop loop loop loop loop loop loop loop loop", "help me");
        assert!(result.replacement.is_none());
        assert!(result.quality.has_repetition_loop);
    }

    #[test]
    fn guard_blocks_internal_control_protocol_leak() {
        let result = apply_response_guards(
            "<ask_astra_data><query>previous task?</query></ask_astra_data>",
            "hi",
        );
        assert_eq!(
            result.replacement.as_deref(),
            Some(INTERNAL_PROTOCOL_FALLBACK)
        );
        assert!(
            !result
                .replacement
                .as_deref()
                .unwrap()
                .contains("<ask_astra_data>")
        );
    }

    #[test]
    fn guard_passes_clean_text() {
        let result =
            apply_response_guards("Here's what I found in the codebase.", "what did you find?");
        assert!(
            result.replacement.is_none(),
            "clean text should pass all guards"
        );
        assert!(!result.quality.has_fabrication_markers);
        assert!(!result.quality.is_echo);
        assert!(!result.quality.has_repetition_loop);
    }

    #[test]
    fn guard_empty_text_passes() {
        let result = apply_response_guards("", "query");
        assert!(result.replacement.is_none());
    }

    #[test]
    fn guard_returns_quality_for_fabrication() {
        let result = apply_response_guards(
            "Check path/to/your/config.yaml for the settings",
            "where is config?",
        );
        assert!(
            result.replacement.is_none(),
            "fabrication is advisory, not a hard block"
        );
        assert!(result.quality.has_fabrication_markers);
    }

    #[test]
    fn guard_constants_are_user_facing() {
        assert!(
            !PROMPT_LEAK_FALLBACK.is_empty(),
            "fallback must have content"
        );
        assert!(
            !PROMPT_LEAK_FALLBACK.contains("error code"),
            "fallback should be user-friendly"
        );
    }
}
