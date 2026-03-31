//! Merge selected skill bodies into one markdown block for `/chat` `edge_profile` (Phase 0).

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

#[cfg(test)]
mod tests {
    use super::*;

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
}
