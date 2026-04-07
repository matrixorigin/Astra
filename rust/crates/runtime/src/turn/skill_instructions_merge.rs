//! Merge selected skill bodies into one markdown block for `/chat` `edge_profile`.
//!
//! Deprecated: proactive skill injection is retired. Skill activation now goes
//! through the `skill` tool in the agentic loop. This module is kept temporarily
//! for backward compatibility and will be removed.

/// One skill from a selection round: load error, empty body, or usable markdown.
#[derive(Debug, Clone)]
pub struct SkillInstructionLoadOutcome {
    pub skill_name: String,
    pub result: Result<Option<String>, String>,
}

/// Load each skill in order; build merged `## Skill: …` sections and track activation names.
///
/// `load` should return `Err` when instructions cannot be read; `Ok(None)` when the skill has no
/// instruction body (silent skip); `Ok(Some(body))` when a non-empty body exists.
pub fn merge_skill_instruction_bodies_for_chat(
    selected_skills: &[String],
    mut load: impl FnMut(&str) -> Result<Option<String>, String>,
) -> (
    Vec<SkillInstructionLoadOutcome>,
    Option<String>,
    Vec<String>,
) {
    let mut outcomes = Vec::with_capacity(selected_skills.len());
    let mut sections = Vec::new();
    let mut activated = Vec::new();

    for name in selected_skills {
        let result = load(name.as_str());
        if let Ok(Some(ref body)) = result
            && !body.is_empty()
        {
            activated.push(name.clone());
            sections.push(format!("## Skill: {name}\n\n{body}"));
        }
        outcomes.push(SkillInstructionLoadOutcome {
            skill_name: name.clone(),
            result,
        });
    }

    let merged = if sections.is_empty() {
        None
    } else {
        Some(sections.join("\n\n---\n\n"))
    };

    (outcomes, merged, activated)
}

/// Message after the warning glyph when a skill body failed to load (CLI prints `  ⚠ {this}`).
#[must_use]
pub fn skill_instruction_load_failed_message(skill_name: &str, err: &str) -> String {
    format!("Failed to load skill {skill_name}: {err}")
}

/// Comma-separated activated skill names (CLI prints `◆ Using skill: …` with partial color).
#[must_use]
pub fn skill_instruction_activated_names_csv(activated_skills: &[String]) -> String {
    activated_skills.join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stderr_line_shapes() {
        assert_eq!(
            skill_instruction_load_failed_message("x", "boom"),
            "Failed to load skill x: boom"
        );
        assert_eq!(
            skill_instruction_activated_names_csv(&["a".into(), "b".into()]),
            "a, b"
        );
    }

    #[test]
    fn merge_skips_empty_and_collects_errors() {
        let skills = vec!["a".into(), "b".into(), "c".into()];
        let (out, merged, act) = merge_skill_instruction_bodies_for_chat(&skills, |n| match n {
            "a" => Err("nope".into()),
            "b" => Ok(None),
            "c" => Ok(Some("body".into())),
            _ => Ok(None),
        });
        assert_eq!(out.len(), 3);
        assert!(merged.is_some());
        assert!(merged.as_ref().unwrap().contains("## Skill: c"));
        assert_eq!(act, vec!["c".to_string()]);
    }

    // --- edge cases ---

    #[test]
    fn merge_all_errors_returns_none() {
        let skills = vec!["a".into(), "b".into()];
        let (out, merged, act) =
            merge_skill_instruction_bodies_for_chat(&skills, |_| Err("fail".into()));
        assert_eq!(out.len(), 2);
        assert!(merged.is_none());
        assert!(act.is_empty());
    }

    #[test]
    fn merge_all_none_returns_none() {
        let skills = vec!["a".into()];
        let (_, merged, act) =
            merge_skill_instruction_bodies_for_chat(&skills, |_| Ok(None));
        assert!(merged.is_none());
        assert!(act.is_empty());
    }

    #[test]
    fn merge_empty_body_skipped() {
        let skills = vec!["a".into()];
        let (_, merged, act) =
            merge_skill_instruction_bodies_for_chat(&skills, |_| Ok(Some("".into())));
        assert!(merged.is_none());
        assert!(act.is_empty());
    }

    #[test]
    fn merge_empty_skills_vec() {
        let skills: Vec<String> = vec![];
        let (out, merged, act) =
            merge_skill_instruction_bodies_for_chat(&skills, |_| Ok(None));
        assert!(out.is_empty());
        assert!(merged.is_none());
        assert!(act.is_empty());
    }

    #[test]
    fn merge_multiple_bodies_joined_with_separator() {
        let skills = vec!["a".into(), "b".into()];
        let (_, merged, act) = merge_skill_instruction_bodies_for_chat(&skills, |n| {
            Ok(Some(format!("body of {}", n)))
        });
        let text = merged.unwrap();
        assert!(text.contains("## Skill: a"));
        assert!(text.contains("## Skill: b"));
        assert!(text.contains("---")); // separator between sections
        assert_eq!(act.len(), 2);
    }

    #[test]
    fn activated_names_csv_empty() {
        assert_eq!(skill_instruction_activated_names_csv(&[]), "");
    }

    #[test]
    fn activated_names_csv_single() {
        assert_eq!(
            skill_instruction_activated_names_csv(&["solo".into()]),
            "solo"
        );
    }

    #[test]
    fn load_failed_message_unicode() {
        let msg = skill_instruction_load_failed_message("技能A", "找不到文件");
        assert!(msg.contains("技能A"));
        assert!(msg.contains("找不到文件"));
    }
}
