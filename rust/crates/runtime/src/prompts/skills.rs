/// Prefix injected into the user message when `/skill dev <name>` is active.
///
/// The skill source code is embedded so the agent can edit it in-context.
pub fn build_skill_dev_prefix(skill_name: &str, skill_src: &str) -> String {
    format!(
        "[SKILL DEV: {skill_name}]\n\
         Skill source:\n\
         ```json\n\
         {skill_src}\n\
         ```\n\n"
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
