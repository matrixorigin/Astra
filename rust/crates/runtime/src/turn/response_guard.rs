const STRUCTURAL_MARKERS: &[&str] = &[
    "## Core Rules",
    "## Planning Protocol",
    "## Self-Model",
    "## Conversation History",
    "File editing rules:",
    "Tool selection rules:",
    "Reflection rules:",
    "Introspection rules:",
];

const REPEAT_THRESHOLD: usize = 8;

/// Patterns that indicate the LLM fabricated file paths or data.
/// These are common hallucination signatures — paths that look plausible but are invented.
const FABRICATION_MARKERS: &[&str] = &[
    "/path/to/",
    "/example/",
    "example.com/api",
    "<YOUR_",
    "<your_",
    "INSERT_",
    "REPLACE_",
    "xxx",
    "TODO_REPLACE",
];

pub fn is_prompt_leaked(text: &str, fingerprints: &[String]) -> bool {
    if text.is_empty() {
        return false;
    }

    if STRUCTURAL_MARKERS
        .iter()
        .any(|marker| text.contains(marker))
    {
        return true;
    }

    if fingerprints.is_empty() {
        return false;
    }

    let lower = text.to_lowercase();
    fingerprints
        .iter()
        .any(|fingerprint| lower.contains(fingerprint))
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

// ── Tool hallucination detection ────────────────────────────────────

/// Check if tool calls reference tools that don't exist in the allowed set.
/// Returns a list of hallucinated tool names (empty = all valid).
pub fn find_hallucinated_tools(
    tool_calls: &[serde_json::Value],
    allowed_tools: &[&str],
) -> Vec<String> {
    let mut hallucinated = Vec::new();
    for tc in tool_calls {
        let name = tc
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if !name.is_empty() && !allowed_tools.contains(&name) {
            hallucinated.push(name.to_string());
        }
    }
    hallucinated
}

/// Check if tool call arguments are valid JSON.
/// Returns names of tools with malformed arguments.
pub fn find_malformed_args(tool_calls: &[serde_json::Value]) -> Vec<String> {
    let mut malformed = Vec::new();
    for tc in tool_calls {
        let name = tc
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("?")
            .to_string();
        if let Some(args) = tc.get("arguments") {
            // Arguments can be a JSON string (needs parsing) or already an object
            match args {
                serde_json::Value::String(s) => {
                    if !s.is_empty() && serde_json::from_str::<serde_json::Value>(s).is_err() {
                        malformed.push(name);
                    }
                }
                serde_json::Value::Object(_) => {} // already valid
                serde_json::Value::Null => {}      // no args, fine
                _ => {
                    malformed.push(name); // unexpected type
                }
            }
        }
    }
    malformed
}

// ── Response quality signals ────────────────────────────────────────

/// Quality issues detected in a response.
#[derive(Debug, Clone, Default)]
pub struct QualityReport {
    /// Tool names that don't exist in the allowed set.
    pub hallucinated_tools: Vec<String>,
    /// Tool names with malformed JSON arguments.
    pub malformed_args: Vec<String>,
    /// Whether the response contains fabricated path/data markers.
    pub has_fabrication_markers: bool,
    /// Whether the response is a non-answer (just the user's question echoed back).
    pub is_echo: bool,
}

impl QualityReport {
    /// True if any quality issue was detected.
    pub fn has_issues(&self) -> bool {
        !self.hallucinated_tools.is_empty()
            || !self.malformed_args.is_empty()
            || self.has_fabrication_markers
            || self.is_echo
    }

    /// Human-readable summary of quality issues for injection into conversation.
    pub fn to_warning(&self) -> Option<String> {
        if !self.has_issues() {
            return None;
        }
        let mut parts = Vec::new();
        if !self.hallucinated_tools.is_empty() {
            parts.push(format!(
                "Unknown tools: {}. Use get_agent_info to check available tools.",
                self.hallucinated_tools.join(", ")
            ));
        }
        if !self.malformed_args.is_empty() {
            parts.push(format!(
                "Malformed arguments for: {}. Fix the JSON and retry.",
                self.malformed_args.join(", ")
            ));
        }
        if self.has_fabrication_markers {
            parts.push(
                "Response may contain placeholder paths. Use real paths from the project.".to_string(),
            );
        }
        if self.is_echo {
            parts.push("You echoed the question instead of answering it. Use tools to find the answer.".to_string());
        }
        Some(format!("⚠ Quality issues: {}", parts.join(" ")))
    }
}

/// Run quality checks on an LLM response + tool calls.
///
/// * `text`          – the LLM's text response (may be empty if tool calls only)
/// * `tool_calls`    – tool calls the LLM wants to make
/// * `allowed_tools` – tool names available this turn
/// * `user_query`    – the user's original message (for echo detection)
pub fn check_response_quality(
    text: &str,
    tool_calls: &[serde_json::Value],
    allowed_tools: &[&str],
    user_query: &str,
) -> QualityReport {
    let hallucinated_tools = find_hallucinated_tools(tool_calls, allowed_tools);
    let malformed_args = find_malformed_args(tool_calls);

    // Fabrication detection: check text response for placeholder patterns
    let has_fabrication_markers = if text.len() > 20 {
        FABRICATION_MARKERS
            .iter()
            .any(|marker| text.contains(marker))
    } else {
        false
    };

    // Echo detection: LLM just repeated user's question
    let is_echo = if !user_query.is_empty()
        && !text.is_empty()
        && tool_calls.is_empty()
        && user_query.len() > 10
    {
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
        hallucinated_tools,
        malformed_args,
        has_fabrication_markers,
        is_echo,
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
        assert!(!is_repetition_loop("the quick brown fox jumps over the lazy dog"));
    }

    #[test]
    fn repetition_loop_empty() {
        assert!(!is_repetition_loop(""));
    }

    #[test]
    fn repetition_loop_short() {
        assert!(!is_repetition_loop("hello hello hello"));
    }

    // ── Tool hallucination ──────────────────────────────────────

    #[test]
    fn hallucinated_tools_detected() {
        let calls = vec![
            serde_json::json!({"name": "bash", "arguments": "{}"}),
            serde_json::json!({"name": "imaginary_tool", "arguments": "{}"}),
            serde_json::json!({"name": "execute_code", "arguments": "{}"}),
        ];
        let allowed = &["bash", "read_file", "grep"];
        let result = find_hallucinated_tools(&calls, allowed);
        assert_eq!(result, vec!["imaginary_tool", "execute_code"]);
    }

    #[test]
    fn no_hallucination_when_all_valid() {
        let calls = vec![
            serde_json::json!({"name": "bash", "arguments": "{}"}),
            serde_json::json!({"name": "grep", "arguments": "{}"}),
        ];
        let allowed = &["bash", "read_file", "grep"];
        assert!(find_hallucinated_tools(&calls, allowed).is_empty());
    }

    #[test]
    fn hallucination_empty_calls() {
        assert!(find_hallucinated_tools(&[], &["bash"]).is_empty());
    }

    // ── Malformed arguments ─────────────────────────────────────

    #[test]
    fn malformed_args_detected() {
        let calls = vec![
            serde_json::json!({"name": "bash", "arguments": "{invalid json"}),
            serde_json::json!({"name": "grep", "arguments": "{\"pattern\": \"test\"}"}),
        ];
        let result = find_malformed_args(&calls);
        assert_eq!(result, vec!["bash"]);
    }

    #[test]
    fn malformed_args_object_is_valid() {
        let calls = vec![
            serde_json::json!({"name": "bash", "arguments": {"command": "ls"}}),
        ];
        assert!(find_malformed_args(&calls).is_empty());
    }

    #[test]
    fn malformed_args_null_is_valid() {
        let calls = vec![serde_json::json!({"name": "bash", "arguments": null})];
        assert!(find_malformed_args(&calls).is_empty());
    }

    #[test]
    fn malformed_args_empty_string_is_valid() {
        let calls = vec![serde_json::json!({"name": "bash", "arguments": ""})];
        assert!(find_malformed_args(&calls).is_empty());
    }

    // ── Fabrication markers ─────────────────────────────────────

    #[test]
    fn fabrication_detected_in_text() {
        let report = check_response_quality(
            "You can find the config at /path/to/config.yaml",
            &[],
            &["bash"],
            "where is the config?",
        );
        assert!(report.has_fabrication_markers);
        assert!(report.has_issues());
    }

    #[test]
    fn fabrication_not_triggered_for_real_paths() {
        let report = check_response_quality(
            "The config is at rust/crates/runtime/src/config.rs",
            &[],
            &["bash"],
            "where is the config?",
        );
        assert!(!report.has_fabrication_markers);
    }

    #[test]
    fn fabrication_not_triggered_for_short_text() {
        let report = check_response_quality("Done.", &[], &["bash"], "fix it");
        assert!(!report.has_fabrication_markers);
    }

    // ── Echo detection ──────────────────────────────────────────

    #[test]
    fn echo_detected() {
        let query = "How does authentication work in this project?";
        let report = check_response_quality(query, &[], &["bash"], query);
        assert!(report.is_echo);
    }

    #[test]
    fn echo_not_triggered_with_real_answer() {
        let report = check_response_quality(
            "Authentication uses JWT tokens stored in cookies.",
            &[],
            &["bash"],
            "How does authentication work?",
        );
        assert!(!report.is_echo);
    }

    #[test]
    fn echo_not_triggered_when_tools_present() {
        let query = "How does authentication work?";
        let calls = vec![serde_json::json!({"name": "grep", "arguments": "{}"})];
        let report = check_response_quality(query, &calls, &["grep"], query);
        assert!(!report.is_echo, "tool calls mean it's not just an echo");
    }

    // ── QualityReport ───────────────────────────────────────────

    #[test]
    fn quality_report_clean() {
        let report = check_response_quality(
            "Here's what I found...",
            &[serde_json::json!({"name": "bash", "arguments": "{}"})],
            &["bash"],
            "list files",
        );
        assert!(!report.has_issues());
        assert!(report.to_warning().is_none());
    }

    #[test]
    fn quality_report_multiple_issues() {
        let calls = vec![
            serde_json::json!({"name": "fake_tool", "arguments": "{bad json"}),
        ];
        let report = check_response_quality(
            "Check /path/to/file for details",
            &calls,
            &["bash"],
            "find file",
        );
        assert!(report.has_issues());
        let warning = report.to_warning().unwrap();
        assert!(warning.contains("Unknown tools"));
        assert!(warning.contains("Malformed arguments"));
        assert!(warning.contains("placeholder paths"));
    }

    #[test]
    fn quality_report_warning_format() {
        let report = QualityReport {
            hallucinated_tools: vec!["invented_tool".to_string()],
            malformed_args: vec![],
            has_fabrication_markers: false,
            is_echo: false,
        };
        let warning = report.to_warning().unwrap();
        assert!(warning.starts_with("⚠ Quality issues:"));
        assert!(warning.contains("invented_tool"));
        assert!(warning.contains("get_agent_info"));
    }
}
