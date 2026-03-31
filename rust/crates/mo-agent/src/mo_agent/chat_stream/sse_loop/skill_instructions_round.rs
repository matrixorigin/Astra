//! Load merged skill instruction text after LLM-based skill selection (CLI rendering + payload).

use crossterm::style::Stylize;

use crate::skill_instructions::SharedSkillRegistry;

/// Returns combined instruction markdown for payload `edge_profile.skill_instructions`, if any.
pub(crate) fn load_skill_instructions_text(
    skill_registry: &SharedSkillRegistry,
    selected_skills: &[String],
    quiet: bool,
) -> Option<String> {
    if selected_skills.is_empty() {
        return None;
    }
    let mut instructions = Vec::new();
    let mut activated_skills = Vec::new();
    if let Ok(mut reg) = skill_registry.try_write() {
        for skill_name in selected_skills {
            if let Err(e) = reg.load_instructions(skill_name) {
                eprintln!("  {} Failed to load skill {}: {}", "⚠".yellow(), skill_name, e);
                continue;
            }
            if let Some(skill) = reg.get(skill_name)
                && let Some(text) = skill.instruction_text()
            {
                activated_skills.push(skill_name.clone());
                instructions.push(format!("## Skill: {skill_name}\n\n{text}"));
            }
        }
    }
    if instructions.is_empty() {
        return None;
    }
    if !quiet {
        eprintln!(
            "  {} Using skill: {}",
            "◆".cyan(),
            activated_skills.join(", ").cyan()
        );
    }
    Some(instructions.join("\n\n---\n\n"))
}

/// Deduplicated union of skill names across turns (for downstream / telemetry).
pub(crate) fn merge_skill_names_track(all_selected: &mut Vec<String>, round_skills: &[String]) {
    for skill_name in round_skills {
        if !all_selected.contains(skill_name) {
            all_selected.push(skill_name.clone());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_skill_names_track_dedupes() {
        let mut v = vec!["a".into()];
        merge_skill_names_track(&mut v, &["b".into(), "a".into()]);
        assert_eq!(v, vec!["a", "b"]);
    }
}
