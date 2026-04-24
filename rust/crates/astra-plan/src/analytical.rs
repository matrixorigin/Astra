//! Analytical plan generation — for evaluative / research questions.
//!
//! The executable plan flow (outline → subtasks → executor) is the wrong
//! shape for inputs like "客观评价是正确的方向吗？" or "compare the two
//! approaches and tell me which is better". For those, the user wants a
//! structured breakdown of *what to investigate*, not a list of file
//! mutations to apply.
//!
//! This module provides a parallel, single-stage pipeline:
//! 1. Build an analytical prompt asking the LLM to decompose the question
//!    into a `ResearchPlan` (a small set of focused sub-questions).
//! 2. Parse the response (reusing `extract_json_robust` for resilience).
//! 3. Render the plan for display in the REPL.
//!
//! No executor invocation. No persisted `PlanModeState`. The output is a
//! one-shot deliverable — the user reads it, optionally pursues each
//! sub-question via normal chat, and moves on.

use serde::{Deserialize, Serialize};

use crate::decompose::{ProjectContext, extract_json_robust};

/// A research plan: a structured decomposition of an analytical question.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ResearchPlan {
    pub goal: String,
    /// Two- to four-sentence summary of how the question will be approached.
    #[serde(default)]
    pub summary: String,
    pub questions: Vec<ResearchQuestion>,
}

/// One sub-question inside a research plan.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ResearchQuestion {
    pub id: String,
    pub title: String,
    /// Why this sub-question matters to the parent goal — keeps the LLM
    /// honest about each item's relevance.
    #[serde(default)]
    pub why_it_matters: String,
    /// Concrete aspects the answer should cover (bullet list).
    #[serde(default)]
    pub key_aspects: Vec<String>,
    /// Where to look (files, modules, docs, references) — optional.
    #[serde(default)]
    pub suggested_investigations: Vec<String>,
}

/// Build the LLM prompt that turns a goal into a `ResearchPlan`.
pub fn analytical_prompt(goal: &str, context: &ProjectContext) -> String {
    let mut p = String::with_capacity(1024);
    p.push_str(
        "You are a senior software architect helping the user think through an \
         analytical / evaluative question. Decompose the question into a small \
         set of focused sub-questions that, once answered, would let the user \
         reach a confident conclusion.\n\n",
    );

    p.push_str("## Project context\n");
    if !context.languages.is_empty() {
        p.push_str(&format!("- Languages: {}\n", context.languages.join(", ")));
    }
    p.push_str(&format!("- Files: ~{}\n", context.source_file_count));
    if let Some(ref branch) = context.git_branch {
        p.push_str(&format!("- Branch: {branch}\n"));
    }
    if !context.key_modules.is_empty() {
        let top: Vec<_> = context
            .key_modules
            .iter()
            .take(5)
            .map(|(path, lines)| format!("{path} ({lines}L)"))
            .collect();
        p.push_str(&format!("- Key files: {}\n", top.join(", ")));
    }

    p.push_str(&format!("\n## Question\n{goal}\n"));

    p.push_str(
        r#"
## Instructions
Return ONLY the following JSON (no markdown, no commentary):
```json
{
  "goal": "<echo of the question>",
  "summary": "2-4 sentences describing how you'll approach this question",
  "questions": [
    {
      "id": "q1",
      "title": "Concise sub-question",
      "why_it_matters": "1 sentence on why answering this is necessary",
      "key_aspects": ["aspect 1", "aspect 2"],
      "suggested_investigations": ["src/relevant.rs", "the X interface in Y"]
    }
  ]
}
```

Rules:
- 3-6 sub-questions. Fewer if the goal is narrow; more only if the goal is broad.
- Each sub-question must be answerable through code reading or reasoning,
  not by running tests or mutating files.
- Order sub-questions by dependency (foundational understanding first).
- Do NOT propose subtasks, file edits, or executor steps — this is a
  research/evaluation plan, not an executable plan.
"#,
    );

    p
}

/// Parse an analytical-plan response from the LLM. Uses
/// [`extract_json_robust`] so trailing-comma / smart-quote / Python-literal
/// noise from the model is handled transparently.
pub fn parse_analytical_response(text: &str) -> Result<ResearchPlan, String> {
    let json_str = extract_json_robust(text);
    let parsed: serde_json::Value = serde_json::from_str(&json_str)
        .map_err(|e| format!("Invalid analytical-plan JSON: {e}"))?;
    if !parsed.is_object() || !parsed.get("questions").is_some_and(|v| v.is_array()) {
        return Err("Expected JSON object with 'questions' array".into());
    }
    let plan: ResearchPlan = serde_json::from_str(&json_str)
        .map_err(|e| format!("Schema mismatch in analytical plan: {e}"))?;
    if plan.questions.is_empty() {
        return Err("Analytical plan must contain at least one sub-question".into());
    }
    Ok(plan)
}

/// Render a research plan for terminal display. The output is plain text
/// (no ANSI codes) so callers can colorize as needed.
pub fn format_research_plan(plan: &ResearchPlan) -> String {
    let mut out = String::with_capacity(512);
    out.push_str(&format!("Research plan for: {}\n", plan.goal));
    if !plan.summary.is_empty() {
        out.push('\n');
        out.push_str(&plan.summary);
        out.push('\n');
    }
    for (i, q) in plan.questions.iter().enumerate() {
        out.push('\n');
        out.push_str(&format!("  {}. [{}] {}\n", i + 1, q.id, q.title));
        if !q.why_it_matters.is_empty() {
            out.push_str(&format!("     why: {}\n", q.why_it_matters));
        }
        for aspect in &q.key_aspects {
            out.push_str(&format!("       • {aspect}\n"));
        }
        if !q.suggested_investigations.is_empty() {
            out.push_str(&format!(
                "     look at: {}\n",
                q.suggested_investigations.join(", ")
            ));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> ProjectContext {
        ProjectContext {
            languages: vec!["Rust".into()],
            source_file_count: 100,
            git_branch: Some("main".into()),
            ..ProjectContext::default()
        }
    }

    #[test]
    fn analytical_prompt_includes_goal_and_context() {
        let p = analytical_prompt("评估当前接口是否合理", &ctx());
        assert!(p.contains("评估当前接口是否合理"));
        assert!(p.contains("Rust"));
        assert!(p.contains("research/evaluation plan"));
    }

    #[test]
    fn parse_analytical_response_minimal_json() {
        let raw = r#"{
            "goal": "evaluate api X",
            "summary": "we will look at X",
            "questions": [
                {"id": "q1", "title": "is X correct", "key_aspects": ["a", "b"]}
            ]
        }"#;
        let plan = parse_analytical_response(raw).expect("valid analytical JSON");
        assert_eq!(plan.questions.len(), 1);
        assert_eq!(plan.questions[0].id, "q1");
        assert_eq!(plan.questions[0].key_aspects, vec!["a", "b"]);
    }

    #[test]
    fn parse_analytical_response_rejects_empty_questions() {
        let raw = r#"{"goal": "x", "questions": []}"#;
        let err = parse_analytical_response(raw).expect_err("must reject empty");
        assert!(err.contains("at least one"));
    }

    #[test]
    fn parse_analytical_response_handles_robust_repair() {
        // Smart quotes + trailing comma + markdown fence — same noise the
        // real LLM emits. Should still parse via extract_json_robust.
        let raw = "```json\n{\u{201C}goal\u{201D}: \u{201C}eval\u{201D}, \
                   \u{201C}questions\u{201D}: [{\u{201C}id\u{201D}: \u{201C}q1\u{201D}, \
                   \u{201C}title\u{201D}: \u{201C}t\u{201D},}],}\n```";
        let plan = parse_analytical_response(raw).expect("robust repair must succeed");
        assert_eq!(plan.questions.len(), 1);
        assert_eq!(plan.questions[0].title, "t");
    }

    #[test]
    fn format_research_plan_is_human_readable() {
        let plan = ResearchPlan {
            goal: "g".into(),
            summary: "s".into(),
            questions: vec![ResearchQuestion {
                id: "q1".into(),
                title: "title".into(),
                why_it_matters: "why".into(),
                key_aspects: vec!["a".into()],
                suggested_investigations: vec!["src/x.rs".into()],
            }],
        };
        let out = format_research_plan(&plan);
        assert!(out.contains("Research plan for: g"));
        assert!(out.contains("[q1] title"));
        assert!(out.contains("why: why"));
        assert!(out.contains("• a"));
        assert!(out.contains("src/x.rs"));
    }
}
