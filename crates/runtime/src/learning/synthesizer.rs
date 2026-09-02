//! Session-end knowledge backflow for durable Memoria L3 records.
//!
//! This module is pure extraction — it does NOT call Memoria. The caller
//! (session_cleanup) does the HTTP call.

/// A single reusable learning extracted from the session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtractedLesson {
    /// "semantic" for reusable knowledge, "episodic" for session summaries.
    pub memory_type: &'static str,
    /// The content to store in Memoria.
    pub content: String,
    /// Memoria trust tier. T3 = inferred (60-day half-life).
    pub trust_tier: &'static str,
}

// ── LLM-Synthesized Lesson Support ────────────────────────────────────────

/// Context bundle for LLM lesson synthesis. Carries the specific signals
/// needed to produce a context-aware lesson instead of a generic template.
#[derive(Debug, Clone)]
pub struct LessonContext {
    /// What signal triggered this lesson (e.g., "tool_failure", "stall", "correction").
    pub signal_type: String,
    /// Tool name if applicable (e.g., "grep").
    pub tool_name: Option<String>,
    /// The specific error message or correction text.
    pub detail: String,
    /// Last 2 user messages (trimmed) for context.
    pub recent_user_messages: Vec<String>,
    /// Current working directory / project name.
    pub project_hint: Option<String>,
}

/// System prompt for lesson synthesis. Kept short (~150 tokens) to
/// minimize cost. The response budget is 100 tokens.
pub const LESSON_SYNTHESIS_PROMPT: &str = "\
You are extracting a reusable lesson from a coding session event. \
Write ONE specific, actionable sentence that a future session should follow. \
Be concrete: name the tool, the repo, the pattern. \
Do NOT hedge (no 'maybe', 'might', 'consider'). \
Do NOT be generic (no 'tighten the plan', 'use alternatives').";

/// Build the user-turn content for lesson synthesis from a LessonContext.
#[must_use]
pub fn build_synthesis_user_prompt(ctx: &LessonContext) -> String {
    let mut prompt = format!("Signal: {}\n", ctx.signal_type);
    if let Some(ref tool) = ctx.tool_name {
        prompt.push_str(&format!("Tool: {tool}\n"));
    }
    prompt.push_str(&format!("Detail: {}\n", ctx.detail));
    if let Some(ref project) = ctx.project_hint {
        prompt.push_str(&format!("Project: {project}\n"));
    }
    if !ctx.recent_user_messages.is_empty() {
        prompt.push_str("Recent user context:\n");
        for msg in ctx.recent_user_messages.iter().take(2) {
            let truncated: String = msg.chars().take(200).collect();
            prompt.push_str(&format!("  - {truncated}\n"));
        }
    }
    prompt.push_str("\nLesson (one sentence):");
    prompt
}

/// Quality gate for LLM-synthesized lessons. Stricter than the template
/// gate: also rejects content that matches known template phrases.
pub fn is_synthesized_lesson_acceptable(text: &str) -> bool {
    if !is_high_quality_lesson(text) {
        return false;
    }
    let lower = text.to_lowercase();
    // Reject known template phrases — synthesis should be more specific.
    const TEMPLATE_BLOCKLIST: &[&str] = &[
        "consider alternatives",
        "tighten the plan",
        "restate scope",
        "split the plan",
        "verify each action",
        "confirm contested choices",
    ];
    !TEMPLATE_BLOCKLIST
        .iter()
        .any(|phrase| lower.contains(phrase))
}

/// Basic quality gate for template-generated lessons: reject too short,
/// too long, or hedging content. Does NOT check the template blocklist
/// (that's only for LLM-synthesized content in `is_synthesized_lesson_acceptable`).
pub fn is_high_quality_lesson(text: &str) -> bool {
    if !(10..=500).contains(&text.len()) {
        return false;
    }
    let lower = text.to_lowercase();
    // Reject hedging — low-confidence observations shouldn't become lessons.
    if lower.contains("maybe")
        || lower.contains("might")
        || lower.contains("not sure")
        || lower.contains("possibly")
        || lower.contains("i think")
    {
        return false;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quality_gate_rejects_short_and_hedging() {
        assert!(!is_high_quality_lesson("hi"));
        assert!(!is_high_quality_lesson("x".repeat(501).as_str()));
        assert!(!is_high_quality_lesson("maybe use rg instead"));
        assert!(!is_high_quality_lesson("I think grep is slow"));
        assert!(is_high_quality_lesson(
            "Use rg instead of grep in this repo"
        ));
    }

    #[test]
    fn synthesis_prompt_includes_all_context_fields() {
        let ctx = LessonContext {
            signal_type: "tool_failure".into(),
            tool_name: Some("grep".into()),
            detail: "timed out after 30s on node_modules".into(),
            recent_user_messages: vec!["search for the config parser".into()],
            project_hint: Some("Astra".into()),
        };
        let prompt = build_synthesis_user_prompt(&ctx);
        assert!(prompt.contains("Signal: tool_failure"));
        assert!(prompt.contains("Tool: grep"));
        assert!(prompt.contains("timed out after 30s"));
        assert!(prompt.contains("Astra"));
        assert!(prompt.contains("search for the config parser"));
        assert!(prompt.contains("Lesson (one sentence):"));
    }

    #[test]
    fn synthesis_prompt_omits_optional_fields() {
        let ctx = LessonContext {
            signal_type: "stall".into(),
            tool_name: None,
            detail: "3 consecutive identical tool calls".into(),
            recent_user_messages: vec![],
            project_hint: None,
        };
        let prompt = build_synthesis_user_prompt(&ctx);
        assert!(!prompt.contains("Tool:"));
        assert!(!prompt.contains("Project:"));
        assert!(!prompt.contains("Recent user"));
    }

    #[test]
    fn synthesis_quality_gate_rejects_templates() {
        assert!(!is_synthesized_lesson_acceptable(
            "consider alternatives to grep"
        ));
        assert!(!is_synthesized_lesson_acceptable("tighten the plan"));
        assert!(!is_synthesized_lesson_acceptable(
            "restate scope before each tool call"
        ));
        assert!(!is_synthesized_lesson_acceptable(
            "split the plan into smaller actions"
        ));
    }

    #[test]
    fn synthesis_quality_gate_accepts_specific_lessons() {
        assert!(is_synthesized_lesson_acceptable(
            "In Astra (280k files), use `rg --glob '!node_modules'` instead of `grep -r`"
        ));
        assert!(is_synthesized_lesson_acceptable(
            "This repo uses pnpm workspaces; always pass --filter to avoid cross-package installs"
        ));
    }

    #[test]
    fn synthesis_quality_gate_inherits_hedging_rejection() {
        assert!(!is_synthesized_lesson_acceptable(
            "Maybe use rg instead of grep in this repo"
        ));
    }

    #[test]
    fn system_prompt_constant_is_non_empty() {
        assert!(!LESSON_SYNTHESIS_PROMPT.is_empty());
        assert!(LESSON_SYNTHESIS_PROMPT.contains("ONE specific"));
    }
}
