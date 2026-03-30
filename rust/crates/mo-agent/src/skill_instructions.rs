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

// ── Progressive Loading Registry ──

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

/// A skill entry that supports progressive loading.
#[derive(Debug, Clone)]
pub struct ProgressiveSkill {
    /// Level 1: Metadata (always loaded).
    pub metadata: SkillMetadata,
    /// Level 2: Full instructions (loaded on demand).
    instructions: Option<SkillInstruction>,
    /// Level 3: Resources (loaded on demand).
    resources: Option<SkillResources>,
    /// Path to skill directory (for lazy loading).
    skill_dir: Option<std::path::PathBuf>,
    /// Current load level.
    pub load_level: SkillLoadLevel,
}

/// Level 3 resources: templates, scripts, and external files.
#[derive(Debug, Clone, Default)]
pub struct SkillResources {
    /// Template files (name -> content).
    pub templates: HashMap<String, String>,
    /// Script files (name -> content).
    pub scripts: HashMap<String, String>,
    /// Total tokens for resources.
    pub resource_tokens: u32,
}

impl ProgressiveSkill {
    /// Create from metadata only (Level 1).
    pub fn from_metadata(metadata: SkillMetadata, skill_dir: Option<std::path::PathBuf>) -> Self {
        Self {
            metadata,
            instructions: None,
            resources: None,
            skill_dir,
            load_level: SkillLoadLevel::Metadata,
        }
    }

    /// Create with full instructions (Level 2).
    pub fn from_instructions(instruction: SkillInstruction, skill_dir: Option<std::path::PathBuf>) -> Self {
        let metadata = SkillMetadata::from(&instruction);
        Self {
            metadata,
            instructions: Some(instruction),
            resources: None,
            skill_dir,
            load_level: SkillLoadLevel::Instructions,
        }
    }

    /// Get skill name.
    pub fn name(&self) -> &str {
        &self.metadata.name
    }

    /// Get description.
    pub fn description(&self) -> &str {
        &self.metadata.description
    }

    /// Get triggers.
    pub fn triggers(&self) -> &[String] {
        &self.metadata.triggers
    }

    /// Check if instructions are loaded.
    pub fn has_instructions(&self) -> bool {
        self.instructions.is_some()
    }

    /// Get instructions if loaded.
    pub fn instructions(&self) -> Option<&SkillInstruction> {
        self.instructions.as_ref()
    }

    /// Get instruction text if loaded.
    pub fn instruction_text(&self) -> Option<&str> {
        self.instructions.as_ref().map(|i| i.instructions.as_str())
    }

    /// Get allowed tools (from instructions if loaded, else empty).
    pub fn allowed_tools(&self) -> Vec<String> {
        self.instructions
            .as_ref()
            .map(|i| i.allowed_tools.clone())
            .unwrap_or_default()
    }

    /// Total tokens currently loaded.
    pub fn loaded_tokens(&self) -> u32 {
        let metadata_tokens = self.metadata.metadata_tokens;
        let instruction_tokens = self.instructions.as_ref().map(|i| i.instruction_tokens).unwrap_or(0);
        let resource_tokens = self.resources.as_ref().map(|r| r.resource_tokens).unwrap_or(0);
        metadata_tokens + instruction_tokens + resource_tokens
    }

    /// Load to Level 2 (instructions) if not already loaded.
    pub fn load_instructions(&mut self) -> Result<(), String> {
        if self.instructions.is_some() {
            return Ok(());
        }

        let skill_dir = self.skill_dir.as_ref()
            .ok_or("No skill directory set for lazy loading")?;

        // Try SKILL.md first
        let skill_md_path = skill_dir.join("SKILL.md");
        if skill_md_path.exists() {
            let content = std::fs::read_to_string(&skill_md_path)
                .map_err(|e| format!("Failed to read SKILL.md: {e}"))?;
            let instruction = parse_skill_md(&content)?;
            self.instructions = Some(instruction);
            self.load_level = SkillLoadLevel::Instructions;
            return Ok(());
        }

        Err("No SKILL.md found for this skill".to_string())
    }

    /// Load to Level 3 (resources) if not already loaded.
    pub fn load_resources(&mut self) -> Result<(), String> {
        // First ensure instructions are loaded
        self.load_instructions()?;

        if self.resources.is_some() {
            return Ok(());
        }

        let skill_dir = self.skill_dir.as_ref()
            .ok_or("No skill directory set for lazy loading")?;

        let mut resources = SkillResources::default();

        // Load templates/ directory
        let templates_dir = skill_dir.join("templates");
        if templates_dir.exists() {
            for entry in std::fs::read_dir(&templates_dir).map_err(|e| e.to_string())? {
                let entry = entry.map_err(|e| e.to_string())?;
                let path = entry.path();
                if path.is_file() {
                    let name = path.file_name().unwrap().to_string_lossy().to_string();
                    let content = std::fs::read_to_string(&path).map_err(|e| e.to_string())?;
                    resources.resource_tokens += (content.len() as u32) / 4;
                    resources.templates.insert(name, content);
                }
            }
        }

        // Load scripts/ directory
        let scripts_dir = skill_dir.join("scripts");
        if scripts_dir.exists() {
            for entry in std::fs::read_dir(&scripts_dir).map_err(|e| e.to_string())? {
                let entry = entry.map_err(|e| e.to_string())?;
                let path = entry.path();
                if path.is_file() {
                    let name = path.file_name().unwrap().to_string_lossy().to_string();
                    let content = std::fs::read_to_string(&path).map_err(|e| e.to_string())?;
                    resources.resource_tokens += (content.len() as u32) / 4;
                    resources.scripts.insert(name, content);
                }
            }
        }

        self.resources = Some(resources);
        self.load_level = SkillLoadLevel::Resources;
        Ok(())
    }

    /// Get resources if loaded.
    pub fn resources(&self) -> Option<&SkillResources> {
        self.resources.as_ref()
    }
}

/// Registry for managing skills with progressive loading.
#[derive(Debug, Default)]
pub struct SkillRegistry {
    /// Skills indexed by name.
    skills: HashMap<String, ProgressiveSkill>,
    /// Total metadata tokens loaded.
    metadata_tokens: u32,
    /// Token budget for metadata (Level 1).
    metadata_budget: u32,
}

impl SkillRegistry {
    /// Create a new skill registry with default budget.
    pub fn new() -> Self {
        Self {
            skills: HashMap::new(),
            metadata_tokens: 0,
            metadata_budget: 10_000, // ~100 skills at ~100 tokens each
        }
    }

    /// Create with custom token budget.
    pub fn with_budget(metadata_budget: u32) -> Self {
        Self {
            skills: HashMap::new(),
            metadata_tokens: 0,
            metadata_budget,
        }
    }

    /// Register a skill at Level 1 (metadata only).
    pub fn register_metadata(&mut self, metadata: SkillMetadata, skill_dir: Option<std::path::PathBuf>) -> Result<(), String> {
        let tokens = metadata.metadata_tokens;
        if self.metadata_tokens + tokens > self.metadata_budget {
            return Err(format!(
                "Metadata budget exceeded: {} + {} > {}",
                self.metadata_tokens, tokens, self.metadata_budget
            ));
        }

        let name = metadata.name.clone();
        let skill = ProgressiveSkill::from_metadata(metadata, skill_dir);
        self.skills.insert(name, skill);
        self.metadata_tokens += tokens;
        Ok(())
    }

    /// Register a skill with full instructions (Level 2).
    pub fn register_instructions(&mut self, instruction: SkillInstruction, skill_dir: Option<std::path::PathBuf>) {
        let name = instruction.name.clone();
        let skill = ProgressiveSkill::from_instructions(instruction, skill_dir);
        // Don't count instruction tokens against metadata budget
        self.metadata_tokens += skill.metadata.metadata_tokens;
        self.skills.insert(name, skill);
    }

    /// Get a skill by name.
    pub fn get(&self, name: &str) -> Option<&ProgressiveSkill> {
        self.skills.get(name)
    }

    /// Get a mutable skill by name.
    pub fn get_mut(&mut self, name: &str) -> Option<&mut ProgressiveSkill> {
        self.skills.get_mut(name)
    }

    /// Find skills matching a trigger keyword.
    pub fn find_by_trigger(&self, trigger: &str) -> Vec<&ProgressiveSkill> {
        let trigger_lower = trigger.to_lowercase();
        self.skills
            .values()
            .filter(|s| s.metadata.triggers.iter().any(|t| t.to_lowercase() == trigger_lower))
            .collect()
    }

    /// List all skill names.
    pub fn skill_names(&self) -> Vec<&str> {
        self.skills.keys().map(|s| s.as_str()).collect()
    }

    /// List all skills.
    pub fn all_skills(&self) -> Vec<&ProgressiveSkill> {
        self.skills.values().collect()
    }

    /// Get total metadata tokens loaded.
    pub fn metadata_tokens(&self) -> u32 {
        self.metadata_tokens
    }

    /// Get remaining metadata budget.
    pub fn remaining_budget(&self) -> u32 {
        self.metadata_budget.saturating_sub(self.metadata_tokens)
    }

    /// Load instructions for a skill (Level 2).
    pub fn load_instructions(&mut self, name: &str) -> Result<(), String> {
        let skill = self.skills.get_mut(name)
            .ok_or_else(|| format!("Skill not found: {name}"))?;
        skill.load_instructions()
    }

    /// Load resources for a skill (Level 3).
    pub fn load_resources(&mut self, name: &str) -> Result<(), String> {
        let skill = self.skills.get_mut(name)
            .ok_or_else(|| format!("Skill not found: {name}"))?;
        skill.load_resources()
    }

    /// Number of registered skills.
    pub fn len(&self) -> usize {
        self.skills.len()
    }

    /// Check if registry is empty.
    pub fn is_empty(&self) -> bool {
        self.skills.is_empty()
    }
}

/// Thread-safe skill registry wrapper.
pub type SharedSkillRegistry = Arc<RwLock<SkillRegistry>>;

/// Create a new shared skill registry.
pub fn new_shared_registry() -> SharedSkillRegistry {
    Arc::new(RwLock::new(SkillRegistry::new()))
}

/// Discover skills from a directory and register metadata only (Level 1).
pub fn discover_and_register_metadata(
    skills_dir: &Path,
    registry: &mut SkillRegistry,
) -> Vec<String> {
    let mut registered = Vec::new();

    let Ok(entries) = std::fs::read_dir(skills_dir) else {
        return registered;
    };

    for entry in entries.flatten() {
        let skill_dir = entry.path();
        if !skill_dir.is_dir() {
            continue;
        }

        let skill_md_path = skill_dir.join("SKILL.md");
        if skill_md_path.exists() {
            if let Ok(content) = std::fs::read_to_string(&skill_md_path) {
                if let Ok(instruction) = parse_skill_md(&content) {
                    let metadata = SkillMetadata::from(&instruction);
                    let name = metadata.name.clone();
                    if registry.register_metadata(metadata, Some(skill_dir)).is_ok() {
                        registered.push(name);
                    }
                }
            }
        }
    }

    registered
}

#[cfg(test)]
mod registry_tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn registry_basic_operations() {
        let mut registry = SkillRegistry::new();
        
        let metadata = SkillMetadata {
            name: "test".to_string(),
            description: "Test skill".to_string(),
            user_invocable: true,
            triggers: vec!["test".to_string()],
            metadata_tokens: 50,
        };
        
        registry.register_metadata(metadata, None).unwrap();
        
        assert_eq!(registry.len(), 1);
        assert!(registry.get("test").is_some());
        assert_eq!(registry.metadata_tokens(), 50);
    }

    #[test]
    fn registry_budget_enforcement() {
        let mut registry = SkillRegistry::with_budget(100);
        
        let metadata1 = SkillMetadata {
            name: "skill1".to_string(),
            description: "First".to_string(),
            user_invocable: true,
            triggers: vec![],
            metadata_tokens: 60,
        };
        
        let metadata2 = SkillMetadata {
            name: "skill2".to_string(),
            description: "Second".to_string(),
            user_invocable: true,
            triggers: vec![],
            metadata_tokens: 60,
        };
        
        registry.register_metadata(metadata1, None).unwrap();
        let result = registry.register_metadata(metadata2, None);
        
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("budget exceeded"));
        assert_eq!(registry.len(), 1);
    }

    #[test]
    fn registry_find_by_trigger() {
        let mut registry = SkillRegistry::new();
        
        registry.register_metadata(SkillMetadata {
            name: "review".to_string(),
            description: "Code review".to_string(),
            user_invocable: true,
            triggers: vec!["review".to_string(), "audit".to_string()],
            metadata_tokens: 50,
        }, None).unwrap();
        
        registry.register_metadata(SkillMetadata {
            name: "debug".to_string(),
            description: "Debug helper".to_string(),
            user_invocable: true,
            triggers: vec!["debug".to_string(), "troubleshoot".to_string()],
            metadata_tokens: 50,
        }, None).unwrap();
        
        let found = registry.find_by_trigger("review");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].name(), "review");
        
        let found = registry.find_by_trigger("AUDIT"); // case-insensitive
        assert_eq!(found.len(), 1);
        
        let found = registry.find_by_trigger("unknown");
        assert!(found.is_empty());
    }

    #[test]
    fn progressive_skill_lazy_loading() {
        let dir = TempDir::new().unwrap();
        let skill_dir = dir.path().join("review");
        std::fs::create_dir_all(&skill_dir).unwrap();
        
        let skill_md = r#"---
name: review
description: "Code review skill"
triggers:
  - review
allowed_tools:
  - read_file
---
# Review Process

1. Check the diff
2. Find issues
"#;
        std::fs::write(skill_dir.join("SKILL.md"), skill_md).unwrap();
        
        // Create at Level 1
        let mut skill = ProgressiveSkill::from_metadata(
            SkillMetadata {
                name: "review".to_string(),
                description: "Code review skill".to_string(),
                user_invocable: true,
                triggers: vec!["review".to_string()],
                metadata_tokens: 50,
            },
            Some(skill_dir),
        );
        
        assert_eq!(skill.load_level, SkillLoadLevel::Metadata);
        assert!(!skill.has_instructions());
        
        // Load to Level 2
        skill.load_instructions().unwrap();
        assert_eq!(skill.load_level, SkillLoadLevel::Instructions);
        assert!(skill.has_instructions());
        assert!(skill.instruction_text().unwrap().contains("Review Process"));
        assert_eq!(skill.allowed_tools(), vec!["read_file"]);
    }

    #[test]
    fn progressive_skill_resource_loading() {
        let dir = TempDir::new().unwrap();
        let skill_dir = dir.path().join("generator");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::create_dir_all(skill_dir.join("templates")).unwrap();
        std::fs::create_dir_all(skill_dir.join("scripts")).unwrap();
        
        let skill_md = r#"---
name: generator
description: "Code generator"
---
Generate code from templates.
"#;
        std::fs::write(skill_dir.join("SKILL.md"), skill_md).unwrap();
        std::fs::write(skill_dir.join("templates/component.tsx"), "export const {{name}} = () => {}").unwrap();
        std::fs::write(skill_dir.join("scripts/setup.sh"), "#!/bin/bash\necho setup").unwrap();
        
        let mut skill = ProgressiveSkill::from_metadata(
            SkillMetadata {
                name: "generator".to_string(),
                description: "Code generator".to_string(),
                user_invocable: true,
                triggers: vec![],
                metadata_tokens: 40,
            },
            Some(skill_dir),
        );
        
        // Load to Level 3
        skill.load_resources().unwrap();
        assert_eq!(skill.load_level, SkillLoadLevel::Resources);
        
        let resources = skill.resources().unwrap();
        assert!(resources.templates.contains_key("component.tsx"));
        assert!(resources.scripts.contains_key("setup.sh"));
        assert!(resources.resource_tokens > 0);
    }

    #[test]
    fn discover_and_register_metadata_works() {
        let dir = TempDir::new().unwrap();
        
        // Create two skills
        for name in ["review", "debug"] {
            let skill_dir = dir.path().join(name);
            std::fs::create_dir_all(&skill_dir).unwrap();
            let content = format!(r#"---
name: {name}
description: "{name} skill"
triggers:
  - {name}
---
Instructions for {name}.
"#);
            std::fs::write(skill_dir.join("SKILL.md"), content).unwrap();
        }
        
        let mut registry = SkillRegistry::new();
        let registered = discover_and_register_metadata(dir.path(), &mut registry);
        
        assert_eq!(registered.len(), 2);
        assert_eq!(registry.len(), 2);
        
        // Should be at Level 1 only
        let skill = registry.get("review").unwrap();
        assert_eq!(skill.load_level, SkillLoadLevel::Metadata);
        assert!(!skill.has_instructions());
    }

    #[test]
    fn loaded_tokens_tracking() {
        let dir = TempDir::new().unwrap();
        let skill_dir = dir.path().join("test");
        std::fs::create_dir_all(&skill_dir).unwrap();
        
        let skill_md = r#"---
name: test
description: "Test skill with long instructions"
---
This is a longer instruction text that should contribute to the token count.
We want to verify that loaded_tokens() returns the correct cumulative total.
"#;
        std::fs::write(skill_dir.join("SKILL.md"), skill_md).unwrap();
        
        let mut skill = ProgressiveSkill::from_metadata(
            SkillMetadata {
                name: "test".to_string(),
                description: "Test".to_string(),
                user_invocable: true,
                triggers: vec![],
                metadata_tokens: 30,
            },
            Some(skill_dir),
        );
        
        // Level 1: metadata only
        let level1_tokens = skill.loaded_tokens();
        assert_eq!(level1_tokens, 30);
        
        // Level 2: instructions loaded
        skill.load_instructions().unwrap();
        let level2_tokens = skill.loaded_tokens();
        assert!(level2_tokens > level1_tokens);
    }

    // ============================================================================
    // Integration Tests - Full pipeline from discovery to invocation
    // ============================================================================

    #[test]
    fn integration_full_skill_lifecycle() {
        // Tests the complete flow: discover -> register -> load instructions -> load resources
        let dir = TempDir::new().unwrap();
        let skill_dir = dir.path().join("my-skill");
        std::fs::create_dir_all(skill_dir.join("templates")).unwrap();
        std::fs::create_dir_all(skill_dir.join("scripts")).unwrap();

        let skill_md = r#"---
name: my-skill
description: "A comprehensive skill for testing"
user_invocable: true
triggers:
  - my-skill
  - custom
allowed_tools:
  - bash
  - read_file
  - write_file
---
# My Skill Instructions

This is a comprehensive skill that demonstrates the full lifecycle.

## Step 1: Setup
- Initialize the environment
- Validate prerequisites

## Step 2: Execute
- Run the main logic
- Handle errors gracefully
"#;
        std::fs::write(skill_dir.join("SKILL.md"), skill_md).unwrap();
        std::fs::write(skill_dir.join("templates/config.yaml"), "key: value").unwrap();
        std::fs::write(skill_dir.join("scripts/init.sh"), "#!/bin/bash\necho init").unwrap();

        // Phase 1: Discover and register metadata only
        let mut registry = SkillRegistry::with_budget(5000);
        let registered = discover_and_register_metadata(dir.path(), &mut registry);
        
        assert_eq!(registered.len(), 1);
        assert_eq!(registered[0], "my-skill");
        
        let skill = registry.get("my-skill").unwrap();
        assert_eq!(skill.load_level, SkillLoadLevel::Metadata);
        assert!(!skill.has_instructions());
        
        // Phase 2: Load instructions on invocation
        let skill = registry.get_mut("my-skill").unwrap();
        skill.load_instructions().unwrap();
        
        assert_eq!(skill.load_level, SkillLoadLevel::Instructions);
        assert!(skill.has_instructions());
        assert!(skill.instruction_text().unwrap().contains("My Skill Instructions"));
        assert_eq!(skill.allowed_tools(), vec!["bash", "read_file", "write_file"]);
        
        // Phase 3: Load resources on demand
        let skill = registry.get_mut("my-skill").unwrap();
        skill.load_resources().unwrap();
        
        assert_eq!(skill.load_level, SkillLoadLevel::Resources);
        let resources = skill.resources().unwrap();
        assert!(resources.templates.contains_key("config.yaml"));
        assert!(resources.scripts.contains_key("init.sh"));
        assert!(resources.templates.get("config.yaml").unwrap().contains("key: value"));
    }

    #[test]
    fn integration_multiple_skills_budget_tracking() {
        let dir = TempDir::new().unwrap();
        
        // Create 5 skills with different token sizes
        let skills_config = [
            ("small", "Small", 10),
            ("medium", "Medium skill with a longer description", 50),
            ("large", "Large skill with very detailed description for testing purposes", 100),
            ("extra", "Extra skill", 20),
            ("final", "Final skill", 15),
        ];
        
        for (name, desc, _) in &skills_config {
            let skill_dir = dir.path().join(name);
            std::fs::create_dir_all(&skill_dir).unwrap();
            let content = format!(r#"---
name: {name}
description: "{desc}"
triggers:
  - {name}
---
Instructions for {name}.
"#);
            std::fs::write(skill_dir.join("SKILL.md"), content).unwrap();
        }
        
        // Register all skills
        let mut registry = SkillRegistry::new();
        discover_and_register_metadata(dir.path(), &mut registry);
        
        assert_eq!(registry.len(), 5);
        
        // Verify total metadata tokens are tracked
        let total_metadata_tokens = registry.metadata_tokens();
        assert!(total_metadata_tokens > 0);
        
        // Verify we can find skills by different triggers
        for (name, _, _) in &skills_config {
            let found = registry.find_by_trigger(name);
            assert_eq!(found.len(), 1, "Should find skill by trigger: {}", name);
        }
    }

    #[test]
    fn integration_skill_without_triggers() {
        let dir = TempDir::new().unwrap();
        let skill_dir = dir.path().join("internal");
        std::fs::create_dir_all(&skill_dir).unwrap();
        
        // Skill with no triggers - can only be invoked explicitly
        let skill_md = r#"---
name: internal
description: "Internal helper skill, not trigger-invocable"
user_invocable: false
---
This skill is used internally and should not be triggered by keywords.
"#;
        std::fs::write(skill_dir.join("SKILL.md"), skill_md).unwrap();
        
        let mut registry = SkillRegistry::new();
        discover_and_register_metadata(dir.path(), &mut registry);
        
        let skill = registry.get("internal").unwrap();
        assert!(!skill.metadata.user_invocable);
        assert!(skill.metadata.triggers.is_empty());
        
        // Should not be found by any trigger
        assert!(registry.find_by_trigger("internal").is_empty());
        assert!(registry.find_by_trigger("helper").is_empty());
    }

    #[test]
    fn integration_empty_resources_directory() {
        let dir = TempDir::new().unwrap();
        let skill_dir = dir.path().join("empty-resources");
        std::fs::create_dir_all(&skill_dir).unwrap();
        // Create empty templates and scripts directories
        std::fs::create_dir_all(skill_dir.join("templates")).unwrap();
        std::fs::create_dir_all(skill_dir.join("scripts")).unwrap();
        
        let skill_md = r#"---
name: empty-resources
description: "Skill with empty resource directories"
---
This skill has templates/ and scripts/ but they are empty.
"#;
        std::fs::write(skill_dir.join("SKILL.md"), skill_md).unwrap();
        
        let mut skill = ProgressiveSkill::from_metadata(
            SkillMetadata {
                name: "empty-resources".to_string(),
                description: "Test".to_string(),
                user_invocable: true,
                triggers: vec![],
                metadata_tokens: 20,
            },
            Some(skill_dir),
        );
        
        skill.load_resources().unwrap();
        
        let resources = skill.resources().unwrap();
        assert!(resources.templates.is_empty());
        assert!(resources.scripts.is_empty());
        assert_eq!(resources.resource_tokens, 0);
    }

    #[test]
    fn integration_flat_resource_files() {
        let dir = TempDir::new().unwrap();
        let skill_dir = dir.path().join("flat");
        std::fs::create_dir_all(skill_dir.join("templates")).unwrap();
        std::fs::create_dir_all(skill_dir.join("scripts")).unwrap();
        
        let skill_md = r#"---
name: flat
description: "Skill with flat resource directories"
---
Flat resources test.
"#;
        std::fs::write(skill_dir.join("SKILL.md"), skill_md).unwrap();
        std::fs::write(skill_dir.join("templates/base.tsx"), "// base template").unwrap();
        std::fs::write(skill_dir.join("templates/App.tsx"), "// React App").unwrap();
        std::fs::write(skill_dir.join("scripts/helper.sh"), "#!/bin/bash\necho helper").unwrap();
        
        let mut skill = ProgressiveSkill::from_metadata(
            SkillMetadata {
                name: "flat".to_string(),
                description: "Test".to_string(),
                user_invocable: true,
                triggers: vec![],
                metadata_tokens: 20,
            },
            Some(skill_dir),
        );
        
        skill.load_resources().unwrap();
        
        let resources = skill.resources().unwrap();
        // Only immediate children are loaded (no nested directories)
        assert!(resources.templates.contains_key("base.tsx"));
        assert!(resources.templates.contains_key("App.tsx"));
        assert!(resources.scripts.contains_key("helper.sh"));
        assert_eq!(resources.templates.len(), 2);
        assert_eq!(resources.scripts.len(), 1);
    }

    #[test]
    fn integration_special_characters_in_description() {
        let dir = TempDir::new().unwrap();
        let skill_dir = dir.path().join("special");
        std::fs::create_dir_all(&skill_dir).unwrap();
        
        let skill_md = r#"---
name: special
description: "Handles \"quotes\", newlines\n, and special chars: <>&"
triggers:
  - special
---
Instructions with special characters: <>&"'
"#;
        std::fs::write(skill_dir.join("SKILL.md"), skill_md).unwrap();
        
        let content = std::fs::read_to_string(skill_dir.join("SKILL.md")).unwrap();
        let result = parse_skill_md(&content);
        
        assert!(result.is_ok());
        let skill = result.unwrap();
        assert!(skill.description.contains("quotes"));
        assert!(skill.instructions.contains("<>&"));
    }

    #[test]
    fn integration_idempotent_loading() {
        let dir = TempDir::new().unwrap();
        let skill_dir = dir.path().join("idempotent");
        std::fs::create_dir_all(skill_dir.join("templates")).unwrap();
        
        let skill_md = r#"---
name: idempotent
description: "Test idempotent loading"
---
Instructions here.
"#;
        std::fs::write(skill_dir.join("SKILL.md"), skill_md).unwrap();
        std::fs::write(skill_dir.join("templates/test.txt"), "template content").unwrap();
        
        let mut skill = ProgressiveSkill::from_metadata(
            SkillMetadata {
                name: "idempotent".to_string(),
                description: "Test".to_string(),
                user_invocable: true,
                triggers: vec![],
                metadata_tokens: 20,
            },
            Some(skill_dir),
        );
        
        // Load instructions multiple times - should be idempotent
        skill.load_instructions().unwrap();
        let tokens_after_first = skill.loaded_tokens();
        
        skill.load_instructions().unwrap();
        let tokens_after_second = skill.loaded_tokens();
        
        assert_eq!(tokens_after_first, tokens_after_second);
        
        // Load resources multiple times
        skill.load_resources().unwrap();
        let tokens_after_resources = skill.loaded_tokens();
        
        skill.load_resources().unwrap();
        let tokens_final = skill.loaded_tokens();
        
        assert_eq!(tokens_after_resources, tokens_final);
    }

    #[test]
    fn integration_registry_all_skills() {
        let dir = TempDir::new().unwrap();
        
        for name in ["skill-a", "skill-b", "skill-c"] {
            let skill_dir = dir.path().join(name);
            std::fs::create_dir_all(&skill_dir).unwrap();
            let content = format!(r#"---
name: {name}
description: "Description for {name}"
user_invocable: true
triggers:
  - {name}
---
Instructions.
"#);
            std::fs::write(skill_dir.join("SKILL.md"), content).unwrap();
        }
        
        let mut registry = SkillRegistry::new();
        discover_and_register_metadata(dir.path(), &mut registry);
        
        let all_skills = registry.all_skills();
        assert_eq!(all_skills.len(), 3);
        
        let names: Vec<_> = all_skills.iter().map(|s| s.name()).collect();
        assert!(names.contains(&"skill-a"));
        assert!(names.contains(&"skill-b"));
        assert!(names.contains(&"skill-c"));
    }

    #[test]
    fn integration_case_insensitive_trigger_matching() {
        let dir = TempDir::new().unwrap();
        let skill_dir = dir.path().join("case-test");
        std::fs::create_dir_all(&skill_dir).unwrap();
        
        let skill_md = r#"---
name: case-test
description: "Test case-insensitive triggers"
triggers:
  - Review
  - CODE_REVIEW
  - checkStyle
---
Test instructions.
"#;
        std::fs::write(skill_dir.join("SKILL.md"), skill_md).unwrap();
        
        let mut registry = SkillRegistry::new();
        discover_and_register_metadata(dir.path(), &mut registry);
        
        // All variations should find the skill
        assert_eq!(registry.find_by_trigger("review").len(), 1);
        assert_eq!(registry.find_by_trigger("REVIEW").len(), 1);
        assert_eq!(registry.find_by_trigger("code_review").len(), 1);
        assert_eq!(registry.find_by_trigger("CODE_REVIEW").len(), 1);
        assert_eq!(registry.find_by_trigger("checkstyle").len(), 1);
        assert_eq!(registry.find_by_trigger("CHECKSTYLE").len(), 1);
    }
}
