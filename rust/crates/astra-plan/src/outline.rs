//! Two-stage plan generation: outline first, then detail.
//!
//! Instead of generating a full plan in one LLM call, this module supports:
//! 1. **Outline** — 2-4 high-level phases (fast, ~3-5s)
//! 2. **Detail** — expand each phase into concrete subtasks
//!
//! This gives the user a chance to review and adjust direction before
//! committing to a full plan.

use serde::{Deserialize, Serialize};

use crate::decompose::ProjectContext;

/// High-level plan outline — generated before the full plan.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanOutline {
    pub phases: Vec<OutlinePhase>,
    pub total_effort: String,
    #[serde(default)]
    pub questions: Vec<crate::decompose::ClarificationQuestion>,
}

/// A single phase in the outline.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutlinePhase {
    pub id: String,
    pub title: String,
    pub description: String,
    #[serde(
        default = "default_one",
        deserialize_with = "crate::decompose::deserialize_coerced_usize"
    )]
    pub estimated_subtasks: usize,
    #[serde(default)]
    pub key_files: Vec<String>,
}

fn default_one() -> usize {
    1
}

/// Build the outline prompt — asks for 2-4 phases, not full subtasks.
pub fn outline_prompt(goal: &str, context: &ProjectContext) -> String {
    let mut prompt = String::with_capacity(1024);

    prompt.push_str(
        "You are a senior software architect. Create a HIGH-LEVEL execution outline.\n\n",
    );

    // Compact project context
    prompt.push_str("## Project\n");
    if !context.languages.is_empty() {
        prompt.push_str(&format!("- Languages: {}\n", context.languages.join(", ")));
    }
    prompt.push_str(&format!("- Files: ~{}\n", context.source_file_count));
    if !context.entry_points.is_empty() {
        prompt.push_str(&format!("- Build: {}\n", context.entry_points.join(", ")));
    }
    if let Some(ref branch) = context.git_branch {
        prompt.push_str(&format!("- Branch: {branch}\n"));
    }
    if !context.key_modules.is_empty() {
        let top: Vec<_> = context
            .key_modules
            .iter()
            .take(5)
            .map(|(p, l)| format!("{p} ({l}L)"))
            .collect();
        prompt.push_str(&format!("- Key files: {}\n", top.join(", ")));
    }

    prompt.push_str(&format!("\n## Goal\n{goal}\n"));

    prompt.push_str(r#"
## Instructions
Create a high-level outline with 2-4 phases. Each phase is a logical stage of work.

If the goal is ambiguous, include questions in the "questions" field instead of guessing.

Return ONLY this JSON:
```json
{
  "phases": [
    {
      "id": "phase-1",
      "title": "Short title",
      "description": "What this phase accomplishes",
      "estimated_subtasks": 2,
      "key_files": ["src/relevant.rs"]
    }
  ],
  "total_effort": "small|medium|large",
  "questions": []
}
```

Rules:
- 2-4 phases, ordered by dependency
- estimated_subtasks: how many concrete subtasks this phase will expand to (1-4 each)
- total_effort: overall project scope
- If you need clarification, put questions in the "questions" array (same format as clarification questions: question, options, default, category)
- No markdown, no explanation — ONLY the JSON
"#);

    prompt
}

/// Parse an outline response from the LLM.
pub fn parse_outline_response(text: &str) -> Result<PlanOutline, String> {
    let json_str = crate::decompose::extract_json_robust(text);

    let parsed: serde_json::Value =
        serde_json::from_str(&json_str).map_err(|e| format!("Invalid JSON: {e}"))?;

    if !parsed.is_object() || !parsed.get("phases").is_some_and(|v| v.is_array()) {
        return Err("Expected JSON object with 'phases' array".into());
    }

    serde_json::from_str::<PlanOutline>(&json_str).map_err(|e| format!("Invalid outline JSON: {e}"))
}

/// Build a detail prompt for expanding one phase into subtasks.
pub fn phase_detail_prompt(
    goal: &str,
    outline: &PlanOutline,
    phase: &OutlinePhase,
    completed_phases: &[String],
    context: &ProjectContext,
) -> String {
    let mut prompt = String::with_capacity(1024);

    prompt.push_str("You are a senior software architect. Expand ONE phase of a plan into concrete subtasks.\n\n");

    prompt.push_str(&format!("## Goal\n{goal}\n\n"));

    // Show full outline for context
    prompt.push_str("## Plan Outline\n");
    for p in &outline.phases {
        let marker = if completed_phases.contains(&p.id) {
            "✓"
        } else if p.id == phase.id {
            "→"
        } else {
            "○"
        };
        prompt.push_str(&format!("{marker} {} — {}\n", p.id, p.title));
    }

    if !completed_phases.is_empty() {
        prompt.push_str(&format!(
            "\nCompleted phases: {}\n",
            completed_phases.join(", ")
        ));
    }

    prompt.push_str(&format!(
        "\n## Expand Phase: {} — {}\n{}\n",
        phase.id, phase.title, phase.description
    ));

    if !phase.key_files.is_empty() {
        prompt.push_str(&format!("Key files: {}\n", phase.key_files.join(", ")));
    }

    // Compact project context
    if !context.languages.is_empty() {
        prompt.push_str(&format!(
            "\nProject: {} · ~{} files",
            context.languages.join("/"),
            context.source_file_count
        ));
        if let Some(ref tf) = context.test_framework {
            prompt.push_str(&format!(" · tests: {tf}"));
        }
        prompt.push('\n');
    }

    prompt.push_str(
        r#"
## Instructions
Generate subtasks for THIS PHASE ONLY. Return ONLY this JSON:
```json
{
  "subtasks": [
    {
      "id": "kebab-case-id",
      "title": "Short title",
      "description": "What to do",
      "depends_on": [],
      "effort": "small|medium|large",
      "files": ["src/file.rs"],
      "acceptance_checks": [
        {"kind": "file_exists", "paths": ["src/file.rs"]}
      ]
    }
  ]
}
```

Rules:
- 1-4 subtasks for this phase
- IDs must be unique across all phases (prefix with phase id if needed)
- depends_on can reference subtask IDs from completed phases
- All paths relative to project root
- Each subtask needs at least one acceptance_check
- No markdown, no explanation — ONLY the JSON
"#,
    );

    prompt
}

/// Format an outline for terminal display.
pub fn format_outline_display(outline: &PlanOutline, goal: &str) -> String {
    let mut out = String::new();
    out.push_str(&format!("  Plan: {goal}\n"));
    out.push_str(&format!(
        "  Effort: {}  ·  {} phases\n\n",
        outline.total_effort,
        outline.phases.len(),
    ));

    for (i, phase) in outline.phases.iter().enumerate() {
        out.push_str(&format!(
            "  {}. {} — {}\n",
            i + 1,
            phase.title,
            phase.description
        ));
        out.push_str(&format!("     ~{} subtasks", phase.estimated_subtasks));
        if !phase.key_files.is_empty() {
            let files: Vec<_> = phase.key_files.iter().take(3).map(|f| f.as_str()).collect();
            out.push_str(&format!("  ·  {}", files.join(", ")));
        }
        out.push('\n');
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_outline_basic() {
        let text = r#"{"phases": [
            {"id": "p1", "title": "Setup", "description": "Init project", "estimated_subtasks": 2, "key_files": ["Cargo.toml"]},
            {"id": "p2", "title": "Implement", "description": "Core logic", "estimated_subtasks": 3, "key_files": []}
        ], "total_effort": "medium", "questions": []}"#;

        let outline = parse_outline_response(text).unwrap();
        assert_eq!(outline.phases.len(), 2);
        assert_eq!(outline.phases[0].id, "p1");
        assert_eq!(outline.phases[1].estimated_subtasks, 3);
        assert_eq!(outline.total_effort, "medium");
    }

    #[test]
    fn parse_outline_with_markdown_fence() {
        let text = "Here's the outline:\n```json\n{\"phases\": [{\"id\": \"p1\", \"title\": \"A\", \"description\": \"B\", \"estimated_subtasks\": 1}], \"total_effort\": \"small\"}\n```";
        let outline = parse_outline_response(text).unwrap();
        assert_eq!(outline.phases.len(), 1);
    }

    #[test]
    fn parse_outline_with_trailing_comma() {
        let text = r#"{"phases": [{"id": "p1", "title": "A", "description": "B", "estimated_subtasks": 1,},], "total_effort": "small",}"#;
        let outline = parse_outline_response(text).unwrap();
        assert_eq!(outline.phases.len(), 1);
    }

    #[test]
    fn parse_outline_with_questions() {
        let text = r#"{"phases": [], "total_effort": "unknown", "questions": [
            {"question": "What scope?", "options": ["A", "B"], "default": 0, "category": "scope"}
        ]}"#;
        let outline = parse_outline_response(text).unwrap();
        assert!(outline.phases.is_empty());
        assert_eq!(outline.questions.len(), 1);
    }

    #[test]
    fn parse_outline_invalid_json() {
        assert!(parse_outline_response("not json").is_err());
    }

    #[test]
    fn parse_outline_missing_phases() {
        assert!(parse_outline_response(r#"{"total_effort": "small"}"#).is_err());
    }

    #[test]
    fn outline_prompt_includes_goal_and_context() {
        let ctx = ProjectContext {
            languages: vec!["Rust".into()],
            source_file_count: 100,
            ..Default::default()
        };
        let prompt = outline_prompt("Add auth", &ctx);
        assert!(prompt.contains("Add auth"));
        assert!(prompt.contains("Rust"));
        assert!(prompt.contains("phases"));
    }

    #[test]
    fn phase_detail_prompt_shows_outline_context() {
        let outline = PlanOutline {
            phases: vec![
                OutlinePhase {
                    id: "p1".into(),
                    title: "Setup".into(),
                    description: "Init".into(),
                    estimated_subtasks: 2,
                    key_files: vec![],
                },
                OutlinePhase {
                    id: "p2".into(),
                    title: "Impl".into(),
                    description: "Code".into(),
                    estimated_subtasks: 3,
                    key_files: vec!["src/lib.rs".into()],
                },
            ],
            total_effort: "medium".into(),
            questions: vec![],
        };
        let ctx = ProjectContext::default();
        let prompt = phase_detail_prompt(
            "Add auth",
            &outline,
            &outline.phases[1],
            &["p1".into()],
            &ctx,
        );
        assert!(prompt.contains("→ p2"));
        assert!(prompt.contains("✓ p1"));
        assert!(prompt.contains("src/lib.rs"));
    }

    #[test]
    fn format_outline_display_basic() {
        let outline = PlanOutline {
            phases: vec![
                OutlinePhase {
                    id: "p1".into(),
                    title: "Setup".into(),
                    description: "Init deps".into(),
                    estimated_subtasks: 2,
                    key_files: vec!["Cargo.toml".into()],
                },
                OutlinePhase {
                    id: "p2".into(),
                    title: "Implement".into(),
                    description: "Core logic".into(),
                    estimated_subtasks: 3,
                    key_files: vec![],
                },
            ],
            total_effort: "medium".into(),
            questions: vec![],
        };
        let display = format_outline_display(&outline, "Add feature");
        assert!(display.contains("Add feature"));
        assert!(display.contains("medium"));
        assert!(display.contains("Setup"));
        assert!(display.contains("Cargo.toml"));
    }

    #[test]
    fn parse_outline_coerces_string_estimated_subtasks() {
        // Models sometimes emit a tech-stack description instead of an integer.
        let json = r#"{
            "phases": [
                {
                    "id": "phase-1",
                    "title": "Create page",
                    "description": "Build the login page",
                    "estimated_subtasks": "Plain HTML/CSS with vanilla JS",
                    "key_files": ["tmp/login.html"]
                }
            ],
            "total_effort": "small",
            "questions": []
        }"#;
        let result = parse_outline_response(json);
        assert!(result.is_ok(), "should coerce string to usize: {result:?}");
        assert_eq!(result.unwrap().phases[0].estimated_subtasks, 1);
    }

    #[test]
    fn parse_outline_coerces_numeric_string_estimated_subtasks() {
        let json = r#"{
            "phases": [{"id":"p1","title":"T","description":"D","estimated_subtasks":"3","key_files":[]}],
            "total_effort": "small"
        }"#;
        let result = parse_outline_response(json);
        assert!(result.is_ok(), "{result:?}");
        assert_eq!(result.unwrap().phases[0].estimated_subtasks, 3);
    }

    #[test]
    fn parse_outline_missing_estimated_subtasks_defaults_to_1() {
        let json = r#"{"phases":[{"id":"p1","title":"T","description":"D","key_files":[]}],"total_effort":"small"}"#;
        let result = parse_outline_response(json);
        assert!(result.is_ok(), "{result:?}");
        assert_eq!(result.unwrap().phases[0].estimated_subtasks, 1);
    }
}
