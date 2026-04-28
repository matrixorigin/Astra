//! SKILL.md parser: parse skill instruction files with YAML frontmatter + Markdown body.
//!
//! This module parses skill instruction files, allowing skills to include
//! detailed, step-by-step guidance that gets injected into the agent's context when the
//! skill is invoked.
//!
//! # File Format
//!
//! ```text
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
//! 1. `{cwd}/.astra/skills/` — project-level
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

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

// ── Skill Discovery Paths ─────────────────────────────────────────────────

/// Standard skill directory search order (high → low priority):
///
/// 1. `{cwd}/.astra/skills/`  — project-level
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
#[deprecated(note = "Use astra_skills::manifest::SkillManifest + LoadedSkill instead")]
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

    // ── Extended fields ──
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
#[deprecated(note = "Use astra_skills::manifest::SkillManifest instead")]
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
    let mut instruction: SkillInstruction = serde_yaml_ng::from_str(yaml_content)
        .map_err(|e| format!("Failed to parse YAML frontmatter: {e}"))?;

    // Set the markdown body
    instruction.instructions = markdown_body.to_string();
    instruction.instruction_tokens = (markdown_body.len() as u32) / 4;

    Ok(instruction)
}

