//! Session-end knowledge backflow — extract reusable learnings from the
//! session's L1b narrative and structure them for durable storage in
//! Memoria's L3 layer (Session Memory Protocol §6.2).
//!
//! This module is the bridge between the session-scoped narrative
//! (Learnings, User Corrections) and cross-session memory (Memoria
//! semantic/episodic memories). It does NOT call Memoria — the caller
//! (session_cleanup) does the HTTP call. This module is pure extraction.

use crate::turn::cloud::session_memory_protocol::SessionMemory;

/// A single reusable learning extracted from the session narrative.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtractedLesson {
    /// "semantic" for reusable knowledge, "episodic" for session summaries.
    pub memory_type: &'static str,
    /// The content to store in Memoria.
    pub content: String,
    /// Memoria trust tier. T3 = inferred (60-day half-life).
    pub trust_tier: &'static str,
}

/// Extract reusable knowledge from the L1b narrative's Learnings and
/// User Corrections sections. Each bullet point becomes a separate
/// `ExtractedLesson` for independent Memoria storage + retrieval.
///
/// Returns empty vec if narrative is None or sections are empty.
pub fn extract_learnings_for_backflow(narrative: Option<&SessionMemory>) -> Vec<ExtractedLesson> {
    let Some(narrative) = narrative else {
        return Vec::new();
    };
    let mut lessons = Vec::new();

    // User Corrections: highest value — the user explicitly told us what's wrong.
    // Store as T2 (curated, 180-day half-life) because human-verified.
    if let Some(corrections) = narrative.section("User Corrections") {
        for line in extract_bullet_points(corrections) {
            if is_synthesized_lesson_acceptable(line) {
                lessons.push(ExtractedLesson {
                    memory_type: "semantic",
                    content: format!("🔧 CORRECTION: {line}"),
                    trust_tier: "T2",
                });
            }
        }
    }

    // Learnings: patterns, gotchas, conventions discovered during the session.
    // Store as T3 (inferred, 60-day half-life) — less certain than corrections.
    if let Some(learnings) = narrative.section("Learnings") {
        for line in extract_bullet_points(learnings) {
            if is_synthesized_lesson_acceptable(line) {
                lessons.push(ExtractedLesson {
                    memory_type: "semantic",
                    content: format!("💡 LESSON: {line}"),
                    trust_tier: "T3",
                });
            }
        }
    }

    lessons
}

/// Build an episodic summary for the session. Stored as `episodic` memory
/// in Memoria so future sessions can retrieve "what happened last time."
pub fn build_episodic_summary(
    session_id: &str,
    turn_count: u32,
    narrative: Option<&SessionMemory>,
) -> Option<ExtractedLesson> {
    if turn_count == 0 {
        return None;
    }
    let task = narrative
        .and_then(|n| n.section("Task Specification"))
        .unwrap_or("(unknown task)");

    let decisions = narrative
        .and_then(|n| n.section("Decisions"))
        .map(|d| {
            let bullets: Vec<&str> = extract_bullet_points(d).into_iter().take(3).collect();
            if bullets.is_empty() {
                String::new()
            } else {
                format!("\nDecisions: {}", bullets.join("; "))
            }
        })
        .unwrap_or_default();

    let content = format!("Session {session_id} ({turn_count} turns): {task}{decisions}");

    Some(ExtractedLesson {
        memory_type: "episodic",
        content,
        trust_tier: "T3",
    })
}

/// Extract bullet points from a markdown section. Handles `- ` and `* `
/// prefixed lines. Trims whitespace, skips empty lines.
fn extract_bullet_points(section: &str) -> Vec<&str> {
    section
        .lines()
        .filter_map(|line| {
            let trimmed = line.trim();
            trimmed
                .strip_prefix("- ")
                .or_else(|| trimmed.strip_prefix("* "))
                .map(str::trim)
                .filter(|s| !s.is_empty())
        })
        .collect()
}

// ── LLM-Synthesized Lesson Support (Phase 4) ─────────────────────────────

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

    fn make_narrative(sections: &[(&str, &str)]) -> SessionMemory {
        let mut text = String::from("[session-memory:v1]\n");
        for (name, content) in sections {
            text.push_str(&format!("# {name}\n{content}\n"));
        }
        SessionMemory::parse(&text).unwrap()
    }

    #[test]
    fn extract_learnings_from_corrections_and_learnings() {
        let narrative = make_narrative(&[
            (
                "User Corrections",
                "- Use rg instead of grep in this repo\n- Always run make check before commit",
            ),
            (
                "Learnings",
                "- This repo has 280k files; grep times out\n- pnpm workspaces require --filter flag",
            ),
        ]);
        let lessons = extract_learnings_for_backflow(Some(&narrative));
        assert_eq!(lessons.len(), 4);
        assert!(lessons[0].content.starts_with("🔧 CORRECTION:"));
        assert_eq!(lessons[0].trust_tier, "T2");
        assert!(lessons[2].content.starts_with("💡 LESSON:"));
        assert_eq!(lessons[2].trust_tier, "T3");
    }

    #[test]
    fn extract_skips_empty_and_none_narrative() {
        assert!(extract_learnings_for_backflow(None).is_empty());
        let empty = make_narrative(&[("Learnings", ""), ("User Corrections", "")]);
        assert!(extract_learnings_for_backflow(Some(&empty)).is_empty());
    }

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
    fn corrections_get_higher_trust_than_learnings() {
        let narrative = make_narrative(&[
            ("User Corrections", "- Always use RS256 not HS256"),
            ("Learnings", "- RS256 is more secure for JWTs"),
        ]);
        let lessons = extract_learnings_for_backflow(Some(&narrative));
        let correction = lessons
            .iter()
            .find(|l| l.content.contains("CORRECTION"))
            .unwrap();
        let learning = lessons
            .iter()
            .find(|l| l.content.contains("LESSON"))
            .unwrap();
        assert_eq!(correction.trust_tier, "T2");
        assert_eq!(learning.trust_tier, "T3");
    }

    #[test]
    fn episodic_summary_includes_task_and_decisions() {
        let narrative = make_narrative(&[
            ("Task Specification", "Add OAuth support to the API"),
            (
                "Decisions",
                "- Use RS256 for JWT signing\n- Store refresh tokens in Redis",
            ),
        ]);
        let summary = build_episodic_summary("sess-123", 15, Some(&narrative)).unwrap();
        assert_eq!(summary.memory_type, "episodic");
        assert!(summary.content.contains("sess-123"));
        assert!(summary.content.contains("15 turns"));
        assert!(summary.content.contains("Add OAuth support"));
        assert!(summary.content.contains("RS256"));
    }

    #[test]
    fn episodic_summary_none_for_zero_turns() {
        assert!(build_episodic_summary("s", 0, None).is_none());
    }

    #[test]
    fn episodic_summary_works_without_narrative() {
        let summary = build_episodic_summary("sess-456", 5, None).unwrap();
        assert!(summary.content.contains("(unknown task)"));
    }

    #[test]
    fn extract_bullet_points_handles_mixed_markers() {
        let section = "- bullet one\n* bullet two\n  - indented bullet\nnot a bullet\n- last";
        let points = extract_bullet_points(section);
        assert_eq!(
            points,
            vec!["bullet one", "bullet two", "indented bullet", "last"]
        );
    }

    // ── Phase 4: LLM synthesis support ─────────────────────────────────────

    #[test]
    fn synthesis_prompt_includes_all_context_fields() {
        let ctx = LessonContext {
            signal_type: "tool_failure".into(),
            tool_name: Some("grep".into()),
            detail: "timed out after 30s on node_modules".into(),
            recent_user_messages: vec!["search for the config parser".into()],
            project_hint: Some("astra-engine".into()),
        };
        let prompt = build_synthesis_user_prompt(&ctx);
        assert!(prompt.contains("Signal: tool_failure"));
        assert!(prompt.contains("Tool: grep"));
        assert!(prompt.contains("timed out after 30s"));
        assert!(prompt.contains("astra-engine"));
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
            "In astra-engine (280k files), use `rg --glob '!node_modules'` instead of `grep -r`"
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
