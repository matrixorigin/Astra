//! LLM-based intent classifier — semantic fallback for the routing engine.
//!
//! When the keyword-based [`crate::routing_engine::RoutingEngine`] produces
//! low confidence (< 0.25) or `TaskType::Unknown`, an optional
//! [`LlmIntentClassifier`] implementation can be invoked to reclassify the
//! user query with actual semantic understanding.
//!
//! This module defines the trait and types only; actual LLM invocation lives
//! in the runtime layer through a trait implementation.

use crate::routing_engine::TaskType;

// ─── Classification Result ───────────────────────────────────────────────────

/// Result of LLM-based intent classification.
#[derive(Debug, Clone)]
pub struct LlmClassification {
    /// Classified task type.
    pub task_type: TaskType,
    /// LLM-assigned confidence (0.0–1.0).
    pub confidence: f64,
    /// Brief reasoning from the LLM (max 1 sentence).
    pub reasoning: String,
}

// ─── Classification Context ──────────────────────────────────────────────────

/// Context passed to the LLM classifier for semantic understanding.
#[derive(Debug, Clone)]
pub struct ClassificationContext {
    /// The user's query.
    pub query: String,
    /// Current conversation turn (1-based).
    pub turn_count: u32,
    /// Tools used in recent turns (for follow-up detection).
    pub recent_tools: Vec<String>,
    /// Memory-derived domain hints.
    pub memory_hints: Vec<String>,
}

// ─── Classifier Trait ────────────────────────────────────────────────────────

/// Trait for LLM-based intent classification.
///
/// Implementations call an actual LLM with the classification prompt produced
/// by [`build_classification_prompt`].
///
/// Returns `None` when the LLM is unavailable or the classification failed —
/// the caller should fall back to the keyword-based result.
pub trait LlmIntentClassifier: Send + Sync {
    /// Classify a user query using LLM semantic understanding.
    ///
    /// Returns `None` on LLM error or unavailability — caller should preserve
    /// the keyword-based classification.
    fn classify(&self, ctx: &ClassificationContext) -> Option<LlmClassification>;
}

// ─── Classification Prompt ───────────────────────────────────────────────────

/// Build the LLM classification prompt for intent detection.
///
/// The prompt asks the LLM to classify the user query into one of the
/// defined [`TaskType`] variants and assign a confidence score. This is a pure
/// function — no side effects, suitable for testing and prompt engineering.
#[must_use]
pub fn build_classification_prompt(ctx: &ClassificationContext) -> String {
    let mut prompt = String::with_capacity(1024);

    prompt.push_str(
        "You are an intent classifier for a coding assistant. Classify the user's query into one of these types:\n\
\n\
- **code**: Writing, editing, refactoring, implementing, debugging code.\n\
- **reasoning**: Explaining, analyzing, comparing, answering \"why\" or \"how\".\n\
- **fetch**: Reading/listing data (PRs, issues, files, CI status, git log).\n\
- **mutate**: Creating/updating/deleting (branches, PRs, commits, issues, files).\n\
- **memory**: Storing/retrieving preferences, bookmarks, tracking (e.g. \"关注\").\n\
- **conversational**: Greetings, chit-chat, simple acknowledgment.\n\
- **compound**: Multiple task types combined (e.g., \"show me PRs and fix the failing one\").\n\
- **unknown**: Cannot determine intent.\n\
\n\
Output ONLY a single JSON object with:\n\
- \"task_type\": one of the 8 types above\n\
- \"confidence\": a number 0.0–1.0 indicating your certainty\n\
- \"reasoning\": a one-sentence explanation of why you chose this type\n\
\n\
Do NOT include any other text, markdown fences, or commentary.\n\
\n\
",
    );

    // Context: turn count
    prompt.push_str(&format!("Turn: {}\n", ctx.turn_count));

    // Context: recent tools
    if !ctx.recent_tools.is_empty() {
        prompt.push_str(&format!("Recent tools: {}\n", ctx.recent_tools.join(", ")));
    }

    // Context: memory hints
    if !ctx.memory_hints.is_empty() {
        prompt.push_str(&format!("Memory hints: {}\n", ctx.memory_hints.join("; ")));
    }

    // The query itself
    prompt.push_str(&format!("\nQuery: \"{}\"\n", ctx.query));

    prompt
}

// ─── Parse LLM Response ─────────────────────────────────────────��────────────

/// Parse the LLM's JSON response into a [`LlmClassification`].
///
/// Returns `None` if the response is malformed or the task_type is unrecognized.
/// This is intentionally strict — a malformed LLM response should not corrupt
/// routing.
#[must_use]
pub fn parse_classification_response(response: &str) -> Option<LlmClassification> {
    // Strip markdown fences if the LLM wrapped the JSON
    let json_str = response.trim();

    let json_str = json_str
        .strip_prefix("```json")
        .or_else(|| json_str.strip_prefix("```"))
        .map(|s| s.strip_suffix("```").unwrap_or(s))
        .unwrap_or(json_str)
        .trim();

    let parsed: serde_json::Value = serde_json::from_str(json_str).ok()?;

    let task_type_str = parsed.get("task_type")?.as_str()?;
    let confidence = parsed.get("confidence")?.as_f64()?;
    let reasoning = parsed
        .get("reasoning")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    let task_type = parse_task_type(task_type_str)?;

    Some(LlmClassification {
        task_type,
        confidence: confidence.clamp(0.0, 1.0),
        reasoning,
    })
}

/// Map a task type string to the [`TaskType`] enum.
fn parse_task_type(s: &str) -> Option<TaskType> {
    match s.to_lowercase().as_str() {
        "code" => Some(TaskType::Code),
        "reasoning" => Some(TaskType::Reasoning),
        "fetch" => Some(TaskType::Fetch),
        "mutate" => Some(TaskType::Mutate),
        "memory" => Some(TaskType::Memory),
        "conversational" => Some(TaskType::Conversational),
        "compound" => Some(TaskType::Compound),
        "unknown" => Some(TaskType::Unknown),
        _ => None,
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_includes_query() {
        let ctx = ClassificationContext {
            query: "will this change introduce a security vulnerability?".into(),
            turn_count: 1,
            recent_tools: vec![],
            memory_hints: vec![],
        };
        let prompt = build_classification_prompt(&ctx);
        assert!(prompt.contains("will this change introduce a security vulnerability?"));
        assert!(prompt.contains("Turn: 1"));
    }

    #[test]
    fn prompt_includes_context() {
        let ctx = ClassificationContext {
            query: "show the diff".into(),
            turn_count: 3,
            recent_tools: vec!["git_diff".into(), "read_file".into()],
            memory_hints: vec!["matrixorigin = GitHub org".into()],
        };
        let prompt = build_classification_prompt(&ctx);
        assert!(prompt.contains("Turn: 3"));
        assert!(prompt.contains("git_diff, read_file"));
        assert!(prompt.contains("matrixorigin = GitHub org"));
    }

    #[test]
    fn parse_valid_response() {
        let response = r#"{"task_type": "reasoning", "confidence": 0.85, "reasoning": "Query asks about security implications of a code change"}"#;
        let classification = parse_classification_response(response).unwrap();
        assert_eq!(classification.task_type, TaskType::Reasoning);
        assert!((classification.confidence - 0.85).abs() < 0.01);
        assert!(!classification.reasoning.is_empty());
    }

    #[test]
    fn parse_response_with_markdown_fence() {
        let response = "```json\n{\"task_type\": \"code\", \"confidence\": 0.9, \"reasoning\": \"User wants to implement a feature\"}\n```";
        let classification = parse_classification_response(response).unwrap();
        assert_eq!(classification.task_type, TaskType::Code);
        assert!((classification.confidence - 0.9).abs() < 0.01);
    }

    #[test]
    fn parse_response_confidence_clamped() {
        let response = r#"{"task_type": "fetch", "confidence": 1.5, "reasoning": "ok"}"#;
        let classification = parse_classification_response(response).unwrap();
        assert_eq!(classification.confidence, 1.0);
    }

    #[test]
    fn parse_response_unknown_type_returns_none() {
        let response = r#"{"task_type": "nonexistent", "confidence": 0.5, "reasoning": "?"}"#;
        assert!(parse_classification_response(response).is_none());
    }

    #[test]
    fn parse_response_malformed_json_returns_none() {
        assert!(parse_classification_response("not json").is_none());
        assert!(parse_classification_response("").is_none());
    }

    #[test]
    fn parse_all_task_types() {
        let types = [
            "code",
            "reasoning",
            "fetch",
            "mutate",
            "memory",
            "conversational",
            "compound",
            "unknown",
        ];
        for t in &types {
            let response = format!(
                r#"{{"task_type": "{}", "confidence": 0.5, "reasoning": ""}}"#,
                t
            );
            assert!(
                parse_classification_response(&response).is_some(),
                "should parse task_type: {t}"
            );
        }
    }
}
