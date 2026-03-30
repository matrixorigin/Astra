//! SKILL.md parser: parse skill instruction files with YAML frontmatter + Markdown body.
//!
//! This module implements Claude Code-style skill instructions, allowing skills to include
//! detailed, step-by-step guidance that gets injected into the agent's context when the
//! skill is invoked.
//!
//! # File Format
//!
//! ```markdown
//! ---
//! name: diagnose
//! description: "Run a structured diagnostic on the current project"
//! user_invocable: true
//! triggers:
//!   - diagnose
//!   - debug
//!   - troubleshoot
//! allowed_tools:
//!   - bash
//!   - read_file
//!   - git_log
//! ---
//! Follow these steps exactly:
//! 1. Identify the symptom: Ask the user to describe the issue
//! 2. Check recent changes: Run `git log --oneline -10`
//! 3. Verify the environment: Check Node, deps, env vars
//! 4. Reproduce: Attempt to break it with a test case
//! ```
//!
//! # Three-Level Loading
//!
//! - **Level 1 (Metadata)**: ~100 tokens - name, description, triggers only
//! - **Level 2 (Instructions)**: Full SKILL.md content loaded on skill invocation
//! - **Level 3 (Resources)**: Templates, scripts, references loaded on demand

use serde::{Deserialize, Serialize};
use std::path::Path;

/// Skill instruction parsed from SKILL.md file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillInstruction {
    /// Unique skill identifier (from frontmatter or filename).
    pub name: String,
    /// Human-readable description of what the skill does.
    pub description: String,
    /// Whether users can manually invoke this skill via `/skill-name`.
    #[serde(default = "default_true")]
    pub user_invocable: bool,
    /// Keywords that trigger automatic skill selection.
    #[serde(default)]
    pub triggers: Vec<String>,
    /// Tools this skill is allowed to use (empty = all tools).
    #[serde(default)]
    pub allowed_tools: Vec<String>,
    /// Markdown body containing step-by-step instructions.
    #[serde(skip)]
    pub instructions: String,
    /// Estimated token count for the instructions.
    #[serde(skip)]
    pub instruction_tokens: u32,
}

fn default_true() -> bool {
    true
}

/// Metadata-only view of a skill (Level 1 loading).
/// Used for discovery and selection without loading full instructions.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillMetadata {
    pub name: String,
    pub description: String,
    pub triggers: Vec<String>,
    pub user_invocable: bool,
    /// Estimated tokens for this metadata (~100 tokens).
    pub metadata_tokens: u32,
}

impl From<&SkillInstruction> for SkillMetadata {
    fn from(skill: &SkillInstruction) -> Self {
        // Estimate tokens: ~4 chars per token
        let text = format!("{} {} {:?}", skill.name, skill.description, skill.triggers);
        let metadata_tokens = (text.len() as u32) / 4;
        
        SkillMetadata {
            name: skill.name.clone(),
            description: skill.description.clone(),
            triggers: skill.triggers.clone(),
            user_invocable: skill.user_invocable,
            metadata_tokens,
        }
    }
}

/// Load level for progressive skill loading.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillLoadLevel {
    /// Level 1: Only metadata (name, description, triggers). ~100 tokens.
    Metadata,
    /// Level 2: Full instructions loaded. Variable tokens.
    Instructions,
    /// Level 3: All resources (templates, scripts) loaded.
    Resources,
}

/// Parse SKILL.md content into a SkillInstruction.
///
/// Extracts YAML frontmatter (between `---` markers) and Markdown body.
pub fn parse_skill_md(content: &str) -> Result<SkillInstruction, String> {
    let content = content.trim();
    
    // Check for YAML frontmatter
    if !content.starts_with("---") {
        return Err("SKILL.md must start with YAML frontmatter (---)".to_string());
    }
    
    // Find the closing frontmatter marker
    let rest = &content[3..];
    let end_marker = rest
        .find("\n---")
        .ok_or("Missing closing frontmatter marker (---)")?;
    
    let yaml_content = &rest[..end_marker].trim();
    let markdown_body = rest[end_marker + 4..].trim();
    
    // Parse YAML frontmatter
    let mut instruction: SkillInstruction = serde_yaml::from_str(yaml_content)
        .map_err(|e| format!("Failed to parse YAML frontmatter: {e}"))?;
    
    // Set the markdown body
    instruction.instructions = markdown_body.to_string();
    instruction.instruction_tokens = (markdown_body.len() as u32) / 4;
    
    Ok(instruction)
}

/// Load a SKILL.md file from disk.
pub fn load_skill_md(path: &Path) -> Result<SkillInstruction, String> {
    let content = std::fs::read_to_string(path)
        .map_err(|e| format!("Failed to read SKILL.md: {e}"))?;
    parse_skill_md(&content)
}

/// Discover all SKILL.md files in a skills directory.
/// Returns `(skill_name, path)` pairs without loading full content.
pub fn discover_skill_instructions(skills_dir: &Path) -> Vec<(String, std::path::PathBuf)> {
    let mut skills = Vec::new();
    let entries = match std::fs::read_dir(skills_dir) {
        Ok(e) => e,
        Err(_) => return skills,
    };
    
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            let skill_md_path = path.join("SKILL.md");
            if skill_md_path.exists() {
                // Use directory name as skill name
                if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                    skills.push((name.to_string(), skill_md_path));
                }
            }
        }
    }
    skills
}

/// Load only metadata from all SKILL.md files (Level 1 loading).
/// This is fast and uses minimal tokens.
pub fn load_skill_metadata(skills_dir: &Path) -> Vec<SkillMetadata> {
    let discoveries = discover_skill_instructions(skills_dir);
    let mut metadata = Vec::new();
    
    for (name, path) in discoveries {
        if let Ok(instruction) = load_skill_md(&path) {
            metadata.push(SkillMetadata::from(&instruction));
        } else {
            // Fallback: create minimal metadata from directory name
            metadata.push(SkillMetadata {
                name,
                description: String::new(),
                triggers: Vec::new(),
                user_invocable: true,
                metadata_tokens: 10,
            });
        }
    }
    metadata
}

/// Load full instructions for a specific skill (Level 2 loading).
pub fn load_skill_instructions(skills_dir: &Path, skill_name: &str) -> Option<SkillInstruction> {
    let skill_path = skills_dir.join(skill_name).join("SKILL.md");
    load_skill_md(&skill_path).ok()
}

// ─── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_valid_skill_md() {
        let content = r#"---
name: diagnose
description: "Run a structured diagnostic"
user_invocable: true
triggers:
  - diagnose
  - debug
allowed_tools:
  - bash
  - read_file
---
Follow these steps:
1. Check the logs
2. Run the tests
"#;
        let skill = parse_skill_md(content).unwrap();
        assert_eq!(skill.name, "diagnose");
        assert_eq!(skill.description, "Run a structured diagnostic");
        assert!(skill.user_invocable);
        assert_eq!(skill.triggers, vec!["diagnose", "debug"]);
        assert_eq!(skill.allowed_tools, vec!["bash", "read_file"]);
        assert!(skill.instructions.contains("Follow these steps"));
        assert!(skill.instructions.contains("Check the logs"));
    }

    #[test]
    fn parse_minimal_skill_md() {
        let content = r#"---
name: simple
description: "A simple skill"
---
Do the thing.
"#;
        let skill = parse_skill_md(content).unwrap();
        assert_eq!(skill.name, "simple");
        assert_eq!(skill.description, "A simple skill");
        assert!(skill.user_invocable); // default true
        assert!(skill.triggers.is_empty());
        assert!(skill.allowed_tools.is_empty());
        assert_eq!(skill.instructions.trim(), "Do the thing.");
    }

    #[test]
    fn parse_missing_frontmatter_fails() {
        let content = "Just some markdown without frontmatter.";
        let result = parse_skill_md(content);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("must start with YAML frontmatter"));
    }

    #[test]
    fn parse_unclosed_frontmatter_fails() {
        let content = r#"---
name: broken
description: "Missing closing marker"
No closing marker here
"#;
        let result = parse_skill_md(content);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Missing closing frontmatter marker"));
    }

    #[test]
    fn parse_invalid_yaml_fails() {
        let content = r#"---
name: [invalid yaml
description: missing bracket
---
Body
"#;
        let result = parse_skill_md(content);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Failed to parse YAML frontmatter"));
    }

    #[test]
    fn skill_metadata_conversion() {
        let skill = SkillInstruction {
            name: "test".to_string(),
            description: "Test skill".to_string(),
            user_invocable: false,
            triggers: vec!["test".to_string(), "check".to_string()],
            allowed_tools: vec!["bash".to_string()],
            instructions: "Long instructions here...".to_string(),
            instruction_tokens: 100,
        };
        
        let metadata = SkillMetadata::from(&skill);
        assert_eq!(metadata.name, "test");
        assert_eq!(metadata.description, "Test skill");
        assert!(!metadata.user_invocable);
        assert_eq!(metadata.triggers, vec!["test", "check"]);
        assert!(metadata.metadata_tokens < 50); // Should be small
    }

    #[test]
    fn instruction_tokens_estimated() {
        let content = r#"---
name: verbose
description: "A skill with lots of instructions"
---
This is a very long set of instructions that should result in a higher token count.
Step 1: Do this thing that requires careful attention to detail.
Step 2: Then do this other thing that also requires attention.
Step 3: Finally, wrap up with this concluding action.
"#;
        let skill = parse_skill_md(content).unwrap();
        // ~300 chars / 4 = ~75 tokens
        assert!(skill.instruction_tokens > 50);
        assert!(skill.instruction_tokens < 150);
    }

    #[test]
    fn multiline_instructions_preserved() {
        let content = r#"---
name: multiline
description: "Test multiline"
---
# Header

This is a paragraph.

```bash
echo "code block"
```

- List item 1
- List item 2
"#;
        let skill = parse_skill_md(content).unwrap();
        assert!(skill.instructions.contains("# Header"));
        assert!(skill.instructions.contains("```bash"));
        assert!(skill.instructions.contains("- List item 1"));
    }
}
