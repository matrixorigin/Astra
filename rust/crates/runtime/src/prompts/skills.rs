/// Prefix injected into the user message when `/skill dev <name>` is active.
///
/// Provides the full skill source in-context plus file path for editing.
/// Prevents the LLM from grep-ing for the file it already has.
/// Writing guidance is kept brief — the LLM already knows markdown;
/// it just needs the structural patterns specific to SKILL.md.
pub fn build_skill_dev_prefix(skill_name: &str, skill_src: &str) -> String {
    format!(
        "[SKILL DEV: {skill_name}]\n\
         The complete source of \"{skill_name}\" is below — do NOT read_file or grep for it.\n\
         File path: `.astra/skills/{skill_name}/SKILL.md` (or `skills/{skill_name}/SKILL.md`)\n\
         To edit: modify the content and use `write_file` to save.\n\n\
         ```markdown\n\
         {skill_src}\n\
         ```\n\n\
         SKILL.md patterns: frontmatter (`when_to_use`, `allowed_tools`, `arguments`), \
         phased steps with success criteria, decision tables, `$ARGUMENTS` placeholder, \
         built-in tools over bash, anti-pattern rules.\n\n"
    )
}

/// A system skill definition: name + instruction block injected into the system prompt.
#[derive(Debug, Clone)]
pub struct SystemSkill {
    pub name: String,
    pub description: String,
    /// The instruction text injected into the system prompt.
    pub instructions: String,
}

/// Built-in markdown output skill.
pub fn builtin_markdown_skill() -> SystemSkill {
    SystemSkill {
        name: "markdown".to_string(),
        description: "Constrain output to well-structured markdown".to_string(),
        instructions: "\
## Output Format: Markdown

All responses MUST follow these formatting rules:
- Use headers (##, ###) to organize sections.
- Use bullet points or numbered lists for multiple items.
- Use code blocks (```) with language tags for code.
- Use **bold** for emphasis on key terms, not ALL CAPS.
- Keep paragraphs short (2-3 sentences max).
- Use tables for comparative data.
- Never output raw unformatted text walls.
- For Chinese content: same rules apply, use markdown structure."
            .to_string(),
    }
}

/// Built-in concise output skill.
pub fn builtin_concise_skill() -> SystemSkill {
    SystemSkill {
        name: "concise".to_string(),
        description: "Keep responses brief and focused".to_string(),
        instructions: "\
## Output Constraint: Concise

- Answer in ≤100 words unless the task requires more.
- Lead with the answer, then explain if needed.
- No filler phrases ('Sure!', 'Of course!', 'Great question!').
- No restating the question.
- For code: show only the relevant diff or snippet, not the full file."
            .to_string(),
    }
}

/// All available built-in system skills.
pub fn builtin_system_skills() -> Vec<SystemSkill> {
    vec![builtin_markdown_skill(), builtin_concise_skill()]
}

/// Build the skill injection block for active system skills.
pub fn build_skill_instructions(skills: &[SystemSkill]) -> String {
    if skills.is_empty() {
        return String::new();
    }
    let mut parts = Vec::new();
    for skill in skills {
        parts.push(format!("{}\n", skill.instructions));
    }
    format!("\n\n{}", parts.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_skill_dev_prefix_contains_name_and_src() {
        let r = build_skill_dev_prefix("my-skill", "# My Skill\nDo stuff");
        assert!(r.contains("my-skill"));
        assert!(r.contains("# My Skill"));
        assert!(r.contains("Do stuff"));
        assert!(r.contains("SKILL DEV"));
    }

    #[test]
    fn builtin_markdown_skill_shape() {
        let s = builtin_markdown_skill();
        assert_eq!(s.name, "markdown");
        assert!(!s.instructions.is_empty());
        assert!(!s.description.is_empty());
    }

    #[test]
    fn builtin_concise_skill_shape() {
        let s = builtin_concise_skill();
        assert_eq!(s.name, "concise");
        assert!(s.instructions.contains("100 words"));
    }

    #[test]
    fn builtin_system_skills_returns_two() {
        let skills = builtin_system_skills();
        assert_eq!(skills.len(), 2);
        let names: Vec<&str> = skills.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"markdown"));
        assert!(names.contains(&"concise"));
    }

    #[test]
    fn build_skill_instructions_empty() {
        assert!(build_skill_instructions(&[]).is_empty());
    }

    #[test]
    fn build_skill_instructions_joins() {
        let skills = builtin_system_skills();
        let r = build_skill_instructions(&skills);
        assert!(r.contains("Markdown"));
        assert!(r.contains("Concise"));
    }
}
