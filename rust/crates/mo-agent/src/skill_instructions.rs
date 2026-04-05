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
//! when_to_use: "Use when the user reports a bug, error, or unexpected behavior"
//! model: "claude-sonnet-4-20250514"
//! max_tokens: 8192
//! ---
//! Follow these steps exactly:
//! 1. Identify the symptom: Ask the user to describe the issue
//! 2. Check recent changes: Run `git log --oneline -10`
//! 3. Verify the environment: Check Node, deps, env vars
//! 4. Reproduce: Attempt to break it with a test case
//! ```
//!
//! # Discovery Paths
//!
//! Skills are discovered from (highest priority first):
//! 1. `{cwd}/.astra/skills/` — project-level (Claude Code–style `.claude/skills`)
//! 2. `{cwd}/skills/` — project-level (explicit)
//! 3. `~/.astra/skills/` — user-level global skills
//!
//! # Three-Level Loading
//!
//! - **Level 1 (Metadata)**: ~100 tokens - name, description, triggers only
//! - **Level 2 (Instructions)**: Full SKILL.md content loaded on skill invocation
//! - **Level 3 (Resources)**: Templates, scripts, references loaded on demand

// Legacy skill types are deprecated in favor of astra_runtime::skills::*.
// Allow dead code and deprecation warnings within this module.
#![allow(dead_code, deprecated)]

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

// ── Skill Discovery Paths ─────────────────────────────────────────────────

/// Standard skill directory search order (high → low priority):
///
/// 1. `{cwd}/.astra/skills/`  — project-level (Claude Code–style .claude/skills)
/// 2. `{cwd}/skills/`         — project-level (legacy / explicit)
/// 3. `~/.astra/skills/`      — user-level global skills
pub fn skill_search_paths() -> Vec<PathBuf> {
    let mut paths = Vec::with_capacity(3);

    if let Ok(cwd) = std::env::current_dir() {
        paths.push(cwd.join(".astra").join("skills")); // 1. project .astra/skills/
        paths.push(cwd.join("skills")); // 2. project skills/
    } else {
        paths.push(PathBuf::from(".astra/skills"));
        paths.push(PathBuf::from("skills"));
    }

    if let Some(home) = dirs::home_dir() {
        paths.push(home.join(".astra").join("skills")); // 3. ~/.astra/skills/
    }

    paths
}

/// Skill instruction parsed from SKILL.md file.
#[deprecated(note = "Use astra_runtime::skills::manifest::SkillManifest + LoadedSkill instead")]
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

    // ── Claude Code–aligned extended fields ──
    /// When this skill should be activated (natural-language hint for the model).
    /// Analogous to CC's `whenToUse` frontmatter field.
    #[serde(default)]
    pub when_to_use: Option<String>,

    /// Model override for this skill (e.g. "claude-sonnet-4-20250514").
    /// If set, turns executed under this skill will request this model.
    #[serde(default)]
    pub model: Option<String>,

    /// Maximum token budget for a single invocation of this skill.
    /// 0 = use system default.
    #[serde(default)]
    pub max_tokens: u32,

    /// Markdown body containing step-by-step instructions.
    #[serde(skip)]
    pub instructions: String,
    /// Estimated token count for the instructions.
    #[serde(skip)]
    pub instruction_tokens: u32,
}

impl Default for SkillInstruction {
    fn default() -> Self {
        Self {
            name: String::new(),
            description: String::new(),
            user_invocable: true, // matches serde default_true
            triggers: Vec::new(),
            allowed_tools: Vec::new(),
            when_to_use: None,
            model: None,
            max_tokens: 0,
            instructions: String::new(),
            instruction_tokens: 0,
        }
    }
}

fn default_true() -> bool {
    true
}

/// Metadata-only view of a skill (Level 1 loading).
/// Used for discovery and selection without loading full instructions.
#[deprecated(note = "Use astra_runtime::skills::manifest::SkillManifest instead")]
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SkillMetadata {
    pub name: String,
    pub description: String,
    pub triggers: Vec<String>,
    pub user_invocable: bool,
    /// When this skill should be activated (from frontmatter `when_to_use`).
    pub when_to_use: Option<String>,
    /// Model override for this skill.
    pub model: Option<String>,
    /// Maximum token budget (0 = system default).
    pub max_tokens: u32,
    /// Estimated tokens for this metadata (~100 tokens).
    pub metadata_tokens: u32,
}

impl From<&SkillInstruction> for SkillMetadata {
    fn from(skill: &SkillInstruction) -> Self {
        // Estimate tokens: ~4 chars per token
        let mut text = format!("{} {} {:?}", skill.name, skill.description, skill.triggers);
        if let Some(ref wtu) = skill.when_to_use {
            text.push(' ');
            text.push_str(wtu);
        }
        let metadata_tokens = (text.len() as u32) / 4;

        SkillMetadata {
            name: skill.name.clone(),
            description: skill.description.clone(),
            triggers: skill.triggers.clone(),
            user_invocable: skill.user_invocable,
            when_to_use: skill.when_to_use.clone(),
            model: skill.model.clone(),
            max_tokens: skill.max_tokens,
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
    let content =
        std::fs::read_to_string(path).map_err(|e| format!("Failed to read SKILL.md: {e}"))?;
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
                ..Default::default()
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
        assert!(
            result
                .unwrap_err()
                .contains("must start with YAML frontmatter")
        );
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
        assert!(
            result
                .unwrap_err()
                .contains("Missing closing frontmatter marker")
        );
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
        assert!(
            result
                .unwrap_err()
                .contains("Failed to parse YAML frontmatter")
        );
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
            ..Default::default()
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

    #[test]
    fn parse_extended_frontmatter_fields() {
        let content = r#"---
name: reviewer
description: "Code review assistant"
when_to_use: "Use when reviewing pull requests or code changes"
model: "claude-sonnet-4-20250514"
max_tokens: 8192
triggers:
  - review
  - pr
---
Review the code carefully.
"#;
        let skill = parse_skill_md(content).unwrap();
        assert_eq!(skill.name, "reviewer");
        assert_eq!(
            skill.when_to_use.as_deref(),
            Some("Use when reviewing pull requests or code changes")
        );
        assert_eq!(skill.model.as_deref(), Some("claude-sonnet-4-20250514"));
        assert_eq!(skill.max_tokens, 8192);

        // Verify metadata conversion includes new fields
        let meta = SkillMetadata::from(&skill);
        assert_eq!(meta.when_to_use, skill.when_to_use);
        assert_eq!(meta.model, skill.model);
    }

    #[test]
    fn extended_fields_default_to_none() {
        let content = r#"---
name: simple
description: "No extended fields"
---
Just instructions.
"#;
        let skill = parse_skill_md(content).unwrap();
        assert!(skill.when_to_use.is_none());
        assert!(skill.model.is_none());
        assert_eq!(skill.max_tokens, 0);
    }

    #[test]
    fn skill_search_paths_includes_astra_dir() {
        let paths = super::skill_search_paths();
        // Should have 3 entries: .astra/skills, skills, ~/.astra/skills
        assert!(paths.len() >= 2);

        let path_strs: Vec<String> = paths.iter().map(|p| p.display().to_string()).collect();
        // First path should end with .astra/skills (project-level)
        assert!(
            path_strs[0].ends_with(".astra/skills"),
            "First path should be .astra/skills, got: {}",
            path_strs[0]
        );
        // Second path should end with /skills (project-level legacy)
        assert!(
            path_strs[1].ends_with("/skills") || path_strs[1] == "skills",
            "Second path should be skills, got: {}",
            path_strs[1]
        );
    }
}

// ── Progressive Loading Registry ──

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

/// A skill entry that supports progressive loading.
#[deprecated(note = "Use astra_runtime::skills::manifest::LoadedSkill instead")]
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
    pub fn from_instructions(
        instruction: SkillInstruction,
        skill_dir: Option<std::path::PathBuf>,
    ) -> Self {
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
        let instruction_tokens = self
            .instructions
            .as_ref()
            .map(|i| i.instruction_tokens)
            .unwrap_or(0);
        let resource_tokens = self
            .resources
            .as_ref()
            .map(|r| r.resource_tokens)
            .unwrap_or(0);
        metadata_tokens + instruction_tokens + resource_tokens
    }

    /// Load to Level 2 (instructions) if not already loaded.
    pub fn load_instructions(&mut self) -> Result<(), String> {
        if self.instructions.is_some() {
            return Ok(());
        }

        let skill_dir = self
            .skill_dir
            .as_ref()
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

        let skill_dir = self
            .skill_dir
            .as_ref()
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
#[deprecated(note = "Use astra_runtime::skills::UnifiedSkillRegistry instead")]
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
    pub fn register_metadata(
        &mut self,
        metadata: SkillMetadata,
        skill_dir: Option<std::path::PathBuf>,
    ) -> Result<(), String> {
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
    pub fn register_instructions(
        &mut self,
        instruction: SkillInstruction,
        skill_dir: Option<std::path::PathBuf>,
    ) {
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
            .filter(|s| {
                s.metadata
                    .triggers
                    .iter()
                    .any(|t| t.to_lowercase() == trigger_lower)
            })
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
        let skill = self
            .skills
            .get_mut(name)
            .ok_or_else(|| format!("Skill not found: {name}"))?;
        skill.load_instructions()
    }

    /// Load resources for a skill (Level 3).
    pub fn load_resources(&mut self, name: &str) -> Result<(), String> {
        let skill = self
            .skills
            .get_mut(name)
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

/// Empty skill registry for contexts where skills are not needed.
/// Uses lazy static initialization.
pub fn empty_registry() -> &'static SharedSkillRegistry {
    static EMPTY: std::sync::LazyLock<SharedSkillRegistry> =
        std::sync::LazyLock::new(|| Arc::new(RwLock::new(SkillRegistry::new())));
    &EMPTY
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
        if skill_md_path.exists()
            && let Ok(content) = std::fs::read_to_string(&skill_md_path)
            && let Ok(instruction) = parse_skill_md(&content)
        {
            let metadata = SkillMetadata::from(&instruction);
            let name = metadata.name.clone();
            if registry
                .register_metadata(metadata, Some(skill_dir))
                .is_ok()
            {
                registered.push(name);
            }
        }
    }

    registered
}

/// Detect skill triggers in a message and return matching skill names.
///
/// This function performs word-level matching - triggers must appear as whole words
/// in the message (case-insensitive). Returns skill names sorted by trigger specificity
/// (longer triggers first).
pub fn detect_triggers_in_message(registry: &SkillRegistry, message: &str) -> Vec<String> {
    let message_lower = message.to_lowercase();
    let words: Vec<&str> = message_lower.split_whitespace().collect();

    let mut matches: Vec<(String, usize)> = Vec::new();

    for skill in registry.all_skills() {
        for trigger in &skill.metadata.triggers {
            let trigger_lower = trigger.to_lowercase();
            // Check if trigger appears as a word in the message
            // Single-word triggers: must match exactly as a word
            // Multi-word triggers (with hyphens/underscores): check word boundary
            if words.contains(&trigger_lower.as_str())
                || is_word_boundary_match(&message_lower, &trigger_lower)
            {
                matches.push((skill.name().to_string(), trigger.len()));
                break; // One match per skill is enough
            }
        }
    }

    // Sort by trigger length (longer = more specific) descending
    matches.sort_by(|a, b| b.1.cmp(&a.1));
    matches.into_iter().map(|(name, _)| name).collect()
}

/// Check if a pattern appears at word boundaries in text.
/// A word boundary is the start/end of text or a non-alphanumeric character.
/// For CJK patterns, we use substring matching since CJK doesn't have word boundaries.
fn is_word_boundary_match(text: &str, pattern: &str) -> bool {
    // For CJK patterns, use simple substring matching
    // CJK characters are self-delimiting (each character is meaningful)
    if pattern.chars().any(is_cjk_char) {
        return text.contains(pattern);
    }

    // For ASCII/Latin patterns, use word boundary matching
    let mut search_start = 0;
    while search_start < text.len() {
        let search_slice = &text[search_start..];
        let Some(pos) = search_slice.find(pattern) else {
            break;
        };

        let abs_pos = search_start + pos;
        let end_pos = abs_pos + pattern.len();

        // Check start boundary (character before match)
        let start_ok = abs_pos == 0 || {
            let prev_slice = &text[..abs_pos];
            prev_slice
                .chars()
                .last()
                .is_none_or(|c| !c.is_ascii_alphanumeric())
        };

        // Check end boundary (character after match)
        let end_ok = end_pos >= text.len() || {
            let next_slice = &text[end_pos..];
            next_slice
                .chars()
                .next()
                .is_none_or(|c| !c.is_ascii_alphanumeric())
        };

        if start_ok && end_ok {
            return true;
        }

        // Move past this occurrence (properly handle UTF-8)
        search_start = abs_pos + text[abs_pos..].chars().next().map_or(1, |c| c.len_utf8());
    }
    false
}

/// Check if a character is CJK (Chinese, Japanese, Korean).
fn is_cjk_char(c: char) -> bool {
    matches!(c,
        '\u{4E00}'..='\u{9FFF}' |  // CJK Unified Ideographs
        '\u{3400}'..='\u{4DBF}' |  // CJK Unified Ideographs Extension A
        '\u{20000}'..='\u{2A6DF}' | // CJK Unified Ideographs Extension B
        '\u{F900}'..='\u{FAFF}' |  // CJK Compatibility Ideographs
        '\u{3000}'..='\u{303F}' |  // CJK Symbols and Punctuation
        '\u{3040}'..='\u{309F}' |  // Hiragana
        '\u{30A0}'..='\u{30FF}' |  // Katakana
        '\u{AC00}'..='\u{D7AF}'    // Hangul Syllables
    )
}

/// Load skill instructions for the first matching trigger in a message.
///
/// Returns the skill name and instruction text if a trigger is found and
/// instructions can be loaded. This performs lazy loading - instructions
/// are only read from disk when needed.
pub fn load_triggered_skill_instructions(
    registry: &mut SkillRegistry,
    message: &str,
) -> Option<(String, String)> {
    // Detect which skills are triggered
    let triggered = {
        // Use immutable borrow for detection
        detect_triggers_in_message(registry, message)
    };

    // Try to load instructions for the first triggered skill
    for skill_name in triggered {
        // Load instructions if not already loaded
        if let Err(e) = registry.load_instructions(&skill_name) {
            eprintln!(
                "  ⚠ Failed to load skill instructions for {}: {}",
                skill_name, e
            );
            continue;
        }

        // Get the instruction text
        if let Some(skill) = registry.get(&skill_name)
            && let Some(text) = skill.instruction_text()
        {
            return Some((skill_name, text.to_string()));
        }
    }

    None
}

// ============================================================================
// Hybrid Skill Detection (Keyword + LLM Fallback)
// ============================================================================

use std::time::{Duration, Instant};

/// Cache entry for LLM skill classification results.
#[derive(Debug, Clone)]
struct ClassificationCacheEntry {
    skill_name: Option<String>,
    timestamp: Instant,
}

/// Cache for LLM skill classification results.
/// Thread-safe wrapper with TTL-based expiration.
pub struct SkillClassificationCache {
    entries: HashMap<String, ClassificationCacheEntry>,
    ttl: Duration,
}

impl SkillClassificationCache {
    /// Create a new cache with the given TTL (default: 5 minutes).
    pub fn new(ttl: Duration) -> Self {
        Self {
            entries: HashMap::new(),
            ttl,
        }
    }

    /// Get a cached classification result if not expired.
    pub fn get(&self, message: &str) -> Option<Option<String>> {
        let key = normalize_message(message);
        if let Some(entry) = self.entries.get(&key)
            && entry.timestamp.elapsed() < self.ttl
        {
            return Some(entry.skill_name.clone());
        }
        None
    }

    /// Cache a classification result.
    pub fn insert(&mut self, message: &str, skill_name: Option<String>) {
        let key = normalize_message(message);
        self.entries.insert(
            key,
            ClassificationCacheEntry {
                skill_name,
                timestamp: Instant::now(),
            },
        );
    }

    /// Clear expired entries.
    pub fn cleanup(&mut self) {
        self.entries
            .retain(|_, entry| entry.timestamp.elapsed() < self.ttl);
    }
}

impl Default for SkillClassificationCache {
    fn default() -> Self {
        Self::new(Duration::from_secs(300)) // 5 minutes
    }
}

/// Normalize a message for cache key comparison.
fn normalize_message(message: &str) -> String {
    message.trim().to_lowercase()
}

/// Check if a message looks like a command/request (vs greeting/statement).
/// Commands typically contain imperative verbs, questions, or task-oriented phrases.
pub fn looks_like_command(message: &str) -> bool {
    let msg_lower = message.to_lowercase();

    // Imperative/request indicators
    let command_patterns = [
        // English imperatives
        "please",
        "help",
        "show",
        "list",
        "find",
        "search",
        "get",
        "check",
        "analyze",
        "evaluate",
        "review",
        "debug",
        "fix",
        "create",
        "make",
        "run",
        "execute",
        "start",
        "stop",
        "update",
        "delete",
        "add",
        // Chinese imperatives
        "请",
        "帮",
        "查",
        "找",
        "看",
        "显示",
        "列出",
        "搜索",
        "获取",
        "分析",
        "评估",
        "审查",
        "调试",
        "修复",
        "创建",
        "运行",
        "执行",
        // Question indicators
        "what",
        "how",
        "why",
        "where",
        "when",
        "which",
        "can you",
        "could you",
        "什么",
        "怎么",
        "为什么",
        "哪",
        "能",
        "可以",
        // Task suffixes
        "一下",
        "吧",
        "下",
    ];

    for pattern in &command_patterns {
        if msg_lower.contains(pattern) {
            return true;
        }
    }

    // Question mark indicates a question/request
    if message.contains('?') || message.contains('？') {
        return true;
    }

    // Very short messages are likely not commands
    if message.len() < 5 {
        return false;
    }

    // Messages with code-like content are likely commands
    if message.contains("```") || message.contains("```") {
        return true;
    }

    false
}

/// Build a classification prompt for the LLM.
pub fn build_classification_prompt(registry: &SkillRegistry, message: &str) -> String {
    let mut skills_desc = String::new();
    for skill in registry.all_skills() {
        if skill.metadata.user_invocable {
            skills_desc.push_str(&format!(
                "- {}: {}\n",
                skill.metadata.name, skill.metadata.description
            ));
        }
    }

    format!(
        r#"You are a skill classifier. Given a user message, determine which skill (if any) should handle it.

Available skills:
{skills_desc}
User message: "{message}"

Reply with ONLY the skill name that best matches the user's intent, or "none" if no skill applies.
Do not explain. Just output the skill name or "none"."#
    )
}

/// Parse the LLM's classification response.
pub fn parse_classification_response(response: &str, registry: &SkillRegistry) -> Option<String> {
    let response_clean = response.trim().to_lowercase();

    // Check for "none" response
    if response_clean == "none" || response_clean.is_empty() {
        return None;
    }

    // Try to match against known skill names
    for skill in registry.all_skills() {
        let skill_name_lower = skill.metadata.name.to_lowercase();
        if response_clean.contains(&skill_name_lower) {
            return Some(skill.metadata.name.clone());
        }
    }

    // If response looks like a skill name but wasn't found, return None
    None
}

/// Hybrid skill detection: keyword match first, then LLM fallback.
///
/// This function tries keyword matching first (fast, free), then falls back
/// to LLM classification for messages that look like commands but didn't
/// match any keyword triggers.
///
/// # Arguments
/// * `registry` - The skill registry
/// * `message` - User message to classify
/// * `cache` - Optional classification cache
/// * `llm_classify` - Async function to call LLM for classification
///
/// # Returns
/// The name of the matched skill, or None if no match.
pub async fn detect_skill_hybrid<F, Fut>(
    registry: &SkillRegistry,
    message: &str,
    cache: Option<&mut SkillClassificationCache>,
    llm_classify: F,
) -> Option<String>
where
    F: FnOnce(String) -> Fut,
    Fut: std::future::Future<Output = Result<String, String>>,
{
    // Stage 1: Try keyword match (fast, free)
    let keyword_matches = detect_triggers_in_message(registry, message);
    if !keyword_matches.is_empty() {
        return Some(keyword_matches[0].clone());
    }

    // Stage 2: Check if message looks like a command
    if !looks_like_command(message) {
        return None;
    }

    // Stage 3: Check cache
    if let Some(cache) = cache.as_ref()
        && let Some(cached_result) = cache.get(message)
    {
        return cached_result;
    }

    // Stage 4: LLM classification
    let prompt = build_classification_prompt(registry, message);
    match llm_classify(prompt).await {
        Ok(response) => {
            let result = parse_classification_response(&response, registry);
            // Cache the result
            if let Some(cache) = cache {
                cache.insert(message, result.clone());
            }
            result
        }
        Err(e) => {
            eprintln!("  ⚠ LLM skill classification failed: {}", e);
            None
        }
    }
}

/// Synchronous version of hybrid detection (keyword only, no LLM fallback).
/// Use this when you can't make async calls.
pub fn detect_skill_hybrid_sync(registry: &SkillRegistry, message: &str) -> Option<String> {
    // Only keyword match in sync mode
    let keyword_matches = detect_triggers_in_message(registry, message);
    if !keyword_matches.is_empty() {
        Some(keyword_matches[0].clone())
    } else {
        None
    }
}

// ─── CLI SkillResolver ──────────────────────────────────────────────────────

/// CLI-side [`SkillResolver`](astra_runtime::turn::skill_tool::SkillResolver)
/// implementation that wraps a [`SharedSkillRegistry`].
///
/// Deprecated: use `UnifiedSkillResolver` from `astra_runtime::skills::registry` instead.
#[deprecated(note = "Use astra_runtime::skills::registry::UnifiedSkillResolver instead")]
pub struct CliSkillResolver {
    registry: SharedSkillRegistry,
}

impl CliSkillResolver {
    pub fn new(registry: SharedSkillRegistry) -> Self {
        Self { registry }
    }
}

impl astra_runtime::turn::skill_tool::SkillResolver for CliSkillResolver {
    fn resolve(
        &self,
        name: &str,
    ) -> Result<astra_runtime::turn::skill_tool::ResolvedSkill, String> {
        // Fast path: read-lock to check if instructions are already loaded.
        // This avoids taking a write lock (and blocking all readers) for the
        // common case where the skill is already at Level 2.
        {
            let reg = self
                .registry
                .read()
                .map_err(|e| format!("skill registry lock poisoned: {e}"))?;
            let skill = reg
                .get(name)
                .ok_or_else(|| format!("unknown skill: {name}"))?;
            if let Some(instruction) = skill.instructions() {
                return Ok(astra_runtime::turn::skill_tool::ResolvedSkill {
                    name: name.to_string(),
                    instructions: instruction.instructions.clone(),
                    model: instruction.model.clone(),
                    max_tokens: if instruction.max_tokens > 0 {
                        Some(instruction.max_tokens)
                    } else {
                        None
                    },
                    allowed_tools: instruction.allowed_tools.clone(),
                    execution_context: astra_runtime::skills::manifest::ExecutionContext::Inline,
                    hooks: astra_runtime::skills::hooks::SkillHooks::default(),
                    skill_dir: None,
                    source: astra_runtime::skills::manifest::SkillSourceKind::Local,
                    success_criteria: Vec::new(),
                    composition: None,
                    input_schema: None,
                });
            }
        }

        // Slow path: instructions not yet loaded. Take write lock to lazy-load.
        // The disk I/O in load_instructions() is unavoidable here, but this only
        // runs once per skill (subsequent calls hit the fast path above).
        let mut reg = self
            .registry
            .write()
            .map_err(|e| format!("skill registry lock poisoned: {e}"))?;

        let skill = reg
            .get_mut(name)
            .ok_or_else(|| format!("unknown skill: {name}"))?;

        skill.load_instructions()?;

        let instruction = skill
            .instructions()
            .ok_or_else(|| format!("skill '{name}' has no instructions after loading"))?;

        Ok(astra_runtime::turn::skill_tool::ResolvedSkill {
            name: name.to_string(),
            instructions: instruction.instructions.clone(),
            model: instruction.model.clone(),
            max_tokens: if instruction.max_tokens > 0 {
                Some(instruction.max_tokens)
            } else {
                None
            },
            allowed_tools: instruction.allowed_tools.clone(),
            execution_context: astra_runtime::skills::manifest::ExecutionContext::Inline,
            hooks: astra_runtime::skills::hooks::SkillHooks::default(),
            skill_dir: None,
            source: astra_runtime::skills::manifest::SkillSourceKind::Local,
            success_criteria: Vec::new(),
            composition: None,
            input_schema: None,
        })
    }

    fn available_skills(&self) -> Vec<astra_runtime::turn::skill_tool::SkillToolInfo> {
        let reg = match self.registry.read() {
            Ok(r) => r,
            Err(_) => return Vec::new(),
        };

        reg.all_skills()
            .iter()
            .filter(|s| s.metadata.user_invocable)
            .map(|s| astra_runtime::turn::skill_tool::SkillToolInfo {
                name: s.name().to_string(),
                description: s.description().to_string(),
                when_to_use: s.metadata.when_to_use.clone(),
                source: astra_runtime::skills::manifest::SkillSourceKind::Local,
            })
            .collect()
    }
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
            ..Default::default()
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
            ..Default::default()
        };

        let metadata2 = SkillMetadata {
            name: "skill2".to_string(),
            description: "Second".to_string(),
            user_invocable: true,
            triggers: vec![],
            metadata_tokens: 60,
            ..Default::default()
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

        registry
            .register_metadata(
                SkillMetadata {
                    name: "review".to_string(),
                    description: "Code review".to_string(),
                    user_invocable: true,
                    triggers: vec!["review".to_string(), "audit".to_string()],
                    metadata_tokens: 50,
                    ..Default::default()
                },
                None,
            )
            .unwrap();

        registry
            .register_metadata(
                SkillMetadata {
                    name: "debug".to_string(),
                    description: "Debug helper".to_string(),
                    user_invocable: true,
                    triggers: vec!["debug".to_string(), "troubleshoot".to_string()],
                    metadata_tokens: 50,
                    ..Default::default()
                },
                None,
            )
            .unwrap();

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
                ..Default::default()
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
        std::fs::write(
            skill_dir.join("templates/component.tsx"),
            "export const {{name}} = () => {}",
        )
        .unwrap();
        std::fs::write(
            skill_dir.join("scripts/setup.sh"),
            "#!/bin/bash\necho setup",
        )
        .unwrap();

        let mut skill = ProgressiveSkill::from_metadata(
            SkillMetadata {
                name: "generator".to_string(),
                description: "Code generator".to_string(),
                user_invocable: true,
                triggers: vec![],
                metadata_tokens: 40,
                ..Default::default()
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
            let content = format!(
                r#"---
name: {name}
description: "{name} skill"
triggers:
  - {name}
---
Instructions for {name}.
"#
            );
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
                ..Default::default()
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
        assert!(
            skill
                .instruction_text()
                .unwrap()
                .contains("My Skill Instructions")
        );
        assert_eq!(
            skill.allowed_tools(),
            vec!["bash", "read_file", "write_file"]
        );

        // Phase 3: Load resources on demand
        let skill = registry.get_mut("my-skill").unwrap();
        skill.load_resources().unwrap();

        assert_eq!(skill.load_level, SkillLoadLevel::Resources);
        let resources = skill.resources().unwrap();
        assert!(resources.templates.contains_key("config.yaml"));
        assert!(resources.scripts.contains_key("init.sh"));
        assert!(
            resources
                .templates
                .get("config.yaml")
                .unwrap()
                .contains("key: value")
        );
    }

    #[test]
    fn integration_multiple_skills_budget_tracking() {
        let dir = TempDir::new().unwrap();

        // Create 5 skills with different token sizes
        let skills_config = [
            ("small", "Small", 10),
            ("medium", "Medium skill with a longer description", 50),
            (
                "large",
                "Large skill with very detailed description for testing purposes",
                100,
            ),
            ("extra", "Extra skill", 20),
            ("final", "Final skill", 15),
        ];

        for (name, desc, _) in &skills_config {
            let skill_dir = dir.path().join(name);
            std::fs::create_dir_all(&skill_dir).unwrap();
            let content = format!(
                r#"---
name: {name}
description: "{desc}"
triggers:
  - {name}
---
Instructions for {name}.
"#
            );
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
                ..Default::default()
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
        std::fs::write(
            skill_dir.join("scripts/helper.sh"),
            "#!/bin/bash\necho helper",
        )
        .unwrap();

        let mut skill = ProgressiveSkill::from_metadata(
            SkillMetadata {
                name: "flat".to_string(),
                description: "Test".to_string(),
                user_invocable: true,
                triggers: vec![],
                metadata_tokens: 20,
                ..Default::default()
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
                ..Default::default()
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
            let content = format!(
                r#"---
name: {name}
description: "Description for {name}"
user_invocable: true
triggers:
  - {name}
---
Instructions.
"#
            );
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

    // ============================================================================
    // Trigger Detection Tests
    // ============================================================================

    #[test]
    fn detect_triggers_simple_word_match() {
        let dir = TempDir::new().unwrap();
        let skill_dir = dir.path().join("review");
        std::fs::create_dir_all(&skill_dir).unwrap();

        let skill_md = r#"---
name: review
description: "Code review skill"
triggers:
  - review
  - code-review
---
Instructions.
"#;
        std::fs::write(skill_dir.join("SKILL.md"), skill_md).unwrap();

        let mut registry = SkillRegistry::new();
        discover_and_register_metadata(dir.path(), &mut registry);

        // Should match when trigger appears as word
        let matches = detect_triggers_in_message(&registry, "please review this PR");
        assert_eq!(matches, vec!["review"]);

        // Should match case-insensitively
        let matches = detect_triggers_in_message(&registry, "REVIEW the changes");
        assert_eq!(matches, vec!["review"]);

        // Should not match partial words
        let matches = detect_triggers_in_message(&registry, "previewing the code");
        assert!(matches.is_empty());
    }

    #[test]
    fn detect_triggers_multiple_skills() {
        let dir = TempDir::new().unwrap();

        // Create two skills with different triggers
        for (name, triggers) in [("review", "review"), ("debug", "debug")] {
            let skill_dir = dir.path().join(name);
            std::fs::create_dir_all(&skill_dir).unwrap();
            let content = format!(
                r#"---
name: {name}
description: "{name} skill"
triggers:
  - {triggers}
---
Instructions for {name}.
"#
            );
            std::fs::write(skill_dir.join("SKILL.md"), content).unwrap();
        }

        let mut registry = SkillRegistry::new();
        discover_and_register_metadata(dir.path(), &mut registry);

        // Should find both when both triggers present
        let matches = detect_triggers_in_message(&registry, "review and debug this code");
        assert_eq!(matches.len(), 2);
        assert!(matches.contains(&"review".to_string()));
        assert!(matches.contains(&"debug".to_string()));

        // Should find only one
        let matches = detect_triggers_in_message(&registry, "please debug this");
        assert_eq!(matches, vec!["debug"]);
    }

    #[test]
    fn detect_triggers_longer_triggers_first() {
        let dir = TempDir::new().unwrap();

        // Create skill with both short and long triggers
        let skill_dir = dir.path().join("review");
        std::fs::create_dir_all(&skill_dir).unwrap();
        let skill_md = r#"---
name: review
description: "Review skill"
triggers:
  - review
  - code-review
  - security-review
---
Instructions.
"#;
        std::fs::write(skill_dir.join("SKILL.md"), skill_md).unwrap();

        let mut registry = SkillRegistry::new();
        discover_and_register_metadata(dir.path(), &mut registry);

        // Longer trigger should be detected (security-review vs review)
        let matches = detect_triggers_in_message(&registry, "do a security-review");
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0], "review");
    }

    #[test]
    fn load_triggered_skill_loads_instructions() {
        let dir = TempDir::new().unwrap();
        let skill_dir = dir.path().join("myskill");
        std::fs::create_dir_all(&skill_dir).unwrap();

        let skill_md = r#"---
name: myskill
description: "My skill"
triggers:
  - analyze
---
# Analysis Steps

1. First step
2. Second step
"#;
        std::fs::write(skill_dir.join("SKILL.md"), skill_md).unwrap();

        let mut registry = SkillRegistry::new();
        discover_and_register_metadata(dir.path(), &mut registry);

        // Initially at metadata level
        assert!(!registry.get("myskill").unwrap().has_instructions());

        // Load via trigger detection
        let result = load_triggered_skill_instructions(&mut registry, "please analyze this");

        assert!(result.is_some());
        let (name, text) = result.unwrap();
        assert_eq!(name, "myskill");
        assert!(text.contains("Analysis Steps"));
        assert!(text.contains("First step"));

        // Should now have instructions loaded
        assert!(registry.get("myskill").unwrap().has_instructions());
    }

    #[test]
    fn load_triggered_skill_no_match() {
        let dir = TempDir::new().unwrap();
        let skill_dir = dir.path().join("review");
        std::fs::create_dir_all(&skill_dir).unwrap();

        let skill_md = r#"---
name: review
description: "Review skill"
triggers:
  - review
---
Instructions.
"#;
        std::fs::write(skill_dir.join("SKILL.md"), skill_md).unwrap();

        let mut registry = SkillRegistry::new();
        discover_and_register_metadata(dir.path(), &mut registry);

        // No trigger match
        let result = load_triggered_skill_instructions(&mut registry, "hello world");
        assert!(result.is_none());
    }

    #[test]
    fn integration_evaluate_session_skill_format() {
        // Test with the actual evaluate-session SKILL.md format
        let dir = TempDir::new().unwrap();
        let skill_dir = dir.path().join("evaluate_session");
        std::fs::create_dir_all(&skill_dir).unwrap();

        // This mirrors the actual skills/evaluate_session/SKILL.md format
        let skill_md = r#"---
name: evaluate-session
description: "Agent self-assessment skill that evaluates performance metrics for a session"
user_invocable: true
triggers:
  - evaluate session
  - session metrics
  - session performance
  - session efficiency
  - how efficient was
  - analyze session
  - session evaluation
  - 评估会话
  - 会话性能
  - 会话效率
allowed_tools:
  - bash
  - read_file
---
# Evaluate Session Skill

When asked to evaluate a session's performance, follow this approach...

## 1. Identify the Target Session
Determine which session to evaluate.

## 2. Gather Session Data
Query the agent_events table.
"#;
        std::fs::write(skill_dir.join("SKILL.md"), skill_md).unwrap();

        let mut registry = SkillRegistry::new();
        discover_and_register_metadata(dir.path(), &mut registry);

        // Verify skill was discovered
        assert_eq!(registry.len(), 1);
        let skill = registry
            .get("evaluate-session")
            .expect("skill should exist");
        assert_eq!(
            skill.metadata.description,
            "Agent self-assessment skill that evaluates performance metrics for a session"
        );
        assert!(skill.metadata.user_invocable);
        assert_eq!(skill.metadata.triggers.len(), 10);

        // Test various trigger phrases - now more precise (session-specific)
        let test_cases = [
            ("evaluate session please", true),
            ("how efficient was I in this session", true),
            ("session metrics analysis", true),
            ("analyze session data", true),
            ("check session performance", true),
            ("会话性能怎么样", true),
            ("评估会话效率", true),
            // These should NOT match (too generic)
            ("evaluate this stock", false),
            ("评估一下股票", false),
            ("performance review", false),
            ("hello world", false),
        ];

        for (message, should_match) in test_cases {
            let matches = detect_triggers_in_message(&registry, message);
            if should_match {
                assert!(
                    !matches.is_empty(),
                    "Expected trigger match for: {}",
                    message
                );
                assert_eq!(matches[0], "evaluate-session");
            } else {
                assert!(matches.is_empty(), "Expected no match for: {}", message);
            }
        }

        // Test full load flow
        let result =
            load_triggered_skill_instructions(&mut registry, "evaluate my session performance");
        assert!(result.is_some());
        let (name, instructions) = result.unwrap();
        assert_eq!(name, "evaluate-session");
        assert!(instructions.contains("Evaluate Session Skill"));
        assert!(instructions.contains("Identify the Target Session"));
    }

    #[test]
    fn integration_real_skills_directory() {
        // Test with the actual skills directory if it exists
        let skills_dir = std::path::Path::new("../../../skills");
        if !skills_dir.exists() {
            // Skip if not running from the expected location
            return;
        }

        let mut registry = SkillRegistry::new();
        let registered = discover_and_register_metadata(skills_dir, &mut registry);

        // We should find at least the example and evaluate_session skills
        assert!(
            !registered.is_empty(),
            "Should find at least 1 skill, found: {:?}",
            registered
        );

        // Check if evaluate-session was found (if SKILL.md exists)
        if registered.contains(&"evaluate-session".to_string()) {
            let skill = registry.get("evaluate-session").unwrap();
            // Triggers are now optional - LLM selects by semantic understanding
            // Just verify the skill has a name and description
            assert!(!skill.metadata.name.is_empty());
            assert!(!skill.metadata.description.is_empty());
        }
    }

    // ============================================================================
    // Hybrid Detection Tests
    // ============================================================================

    #[test]
    fn looks_like_command_english() {
        // Commands should be detected
        assert!(looks_like_command("please help me with this"));
        assert!(looks_like_command("show me the files"));
        assert!(looks_like_command("can you analyze this?"));
        assert!(looks_like_command("what is the status?"));
        assert!(looks_like_command("run the tests"));
        assert!(looks_like_command("debug this error"));

        // Non-commands should not be detected
        assert!(!looks_like_command("hi"));
        assert!(!looks_like_command("ok"));
        assert!(!looks_like_command("yes"));
        assert!(!looks_like_command("no"));
    }

    #[test]
    fn looks_like_command_chinese() {
        // Chinese commands should be detected
        assert!(looks_like_command("请帮我查一下"));
        assert!(looks_like_command("分析一下这个"));
        assert!(looks_like_command("评估一下会话"));
        assert!(looks_like_command("这是什么？"));
        assert!(looks_like_command("怎么解决这个问题"));
        assert!(looks_like_command("运行测试吧"));
    }

    #[test]
    fn classification_cache_basic() {
        let mut cache = SkillClassificationCache::new(Duration::from_secs(60));

        // Insert and retrieve
        cache.insert("test message", Some("skill-a".to_string()));
        assert_eq!(cache.get("test message"), Some(Some("skill-a".to_string())));

        // Case insensitive
        assert_eq!(cache.get("TEST MESSAGE"), Some(Some("skill-a".to_string())));

        // Non-existent
        assert_eq!(cache.get("other message"), None);

        // None values
        cache.insert("no skill", None);
        assert_eq!(cache.get("no skill"), Some(None));
    }

    #[test]
    fn build_classification_prompt_format() {
        let dir = TempDir::new().unwrap();
        let skill_dir = dir.path().join("test-skill");
        std::fs::create_dir_all(&skill_dir).unwrap();

        let skill_md = r#"---
name: test-skill
description: "A test skill"
user_invocable: true
triggers:
  - test
---
Instructions.
"#;
        std::fs::write(skill_dir.join("SKILL.md"), skill_md).unwrap();

        let mut registry = SkillRegistry::new();
        discover_and_register_metadata(dir.path(), &mut registry);

        let prompt = build_classification_prompt(&registry, "run a test");

        assert!(prompt.contains("test-skill"));
        assert!(prompt.contains("A test skill"));
        assert!(prompt.contains("run a test"));
        assert!(prompt.contains("none"));
    }

    #[test]
    fn parse_classification_response_variants() {
        let dir = TempDir::new().unwrap();
        let skill_dir = dir.path().join("my-skill");
        std::fs::create_dir_all(&skill_dir).unwrap();

        let skill_md = r#"---
name: my-skill
description: "My skill"
user_invocable: true
triggers:
  - test
---
Instructions.
"#;
        std::fs::write(skill_dir.join("SKILL.md"), skill_md).unwrap();

        let mut registry = SkillRegistry::new();
        discover_and_register_metadata(dir.path(), &mut registry);

        // Exact match
        assert_eq!(
            parse_classification_response("my-skill", &registry),
            Some("my-skill".to_string())
        );

        // With whitespace
        assert_eq!(
            parse_classification_response("  my-skill  ", &registry),
            Some("my-skill".to_string())
        );

        // Case insensitive
        assert_eq!(
            parse_classification_response("MY-SKILL", &registry),
            Some("my-skill".to_string())
        );

        // None response
        assert_eq!(parse_classification_response("none", &registry), None);
        assert_eq!(parse_classification_response("NONE", &registry), None);
        assert_eq!(parse_classification_response("", &registry), None);

        // Unknown skill
        assert_eq!(
            parse_classification_response("unknown-skill", &registry),
            None
        );
    }

    #[test]
    fn detect_skill_hybrid_sync_keyword_match() {
        let dir = TempDir::new().unwrap();
        let skill_dir = dir.path().join("review");
        std::fs::create_dir_all(&skill_dir).unwrap();

        let skill_md = r#"---
name: review
description: "Code review skill"
user_invocable: true
triggers:
  - review
  - 审查
---
Instructions.
"#;
        std::fs::write(skill_dir.join("SKILL.md"), skill_md).unwrap();

        let mut registry = SkillRegistry::new();
        discover_and_register_metadata(dir.path(), &mut registry);

        // English trigger
        assert_eq!(
            detect_skill_hybrid_sync(&registry, "please review this"),
            Some("review".to_string())
        );

        // Chinese trigger
        assert_eq!(
            detect_skill_hybrid_sync(&registry, "审查一下代码"),
            Some("review".to_string())
        );

        // No match
        assert_eq!(detect_skill_hybrid_sync(&registry, "hello world"), None);
    }
}
