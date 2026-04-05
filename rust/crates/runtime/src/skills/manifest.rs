//! Skill manifest — the universal descriptor for a skill regardless of source.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use super::hooks::SkillHooks;
use super::version::{Dependency, Version};

// Re-export verification types from services for use in skill manifests.
pub use astra_services::{VerificationCriterion, VerifierKind, VerificationResult};

/// Where a skill was loaded from.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SkillSourceKind {
    /// Local filesystem (`SKILL.md` in `.astra/skills/`, `skills/`, `~/.astra/skills/`).
    Local,
    /// Compiled into the binary.
    Bundled,
    /// Server-side skill catalog (MatrixOne `skills_registry`).
    Database,
    /// MCP server (via `skill://` resources).
    Mcp,
    /// External plugin.
    Plugin,
}

impl Default for SkillSourceKind {
    fn default() -> Self {
        SkillSourceKind::Local
    }
}

/// Execution context for a skill invocation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ExecutionContext {
    /// Skill instructions are injected inline into the current conversation context.
    Inline,
    /// Skill runs in an isolated sub-agent loop with separate context and token budget.
    Fork,
}

impl Default for ExecutionContext {
    fn default() -> Self {
        ExecutionContext::Inline
    }
}

/// A named argument that can be passed to a skill.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillArgument {
    /// Argument name (used in `$ARG_NAME` or `${ARG_NAME}` substitution).
    pub name: String,
    /// Human-readable description.
    #[serde(default)]
    pub description: String,
    /// Whether this argument is required.
    #[serde(default)]
    pub required: bool,
    /// Default value if not provided.
    #[serde(default)]
    pub default: Option<String>,
}

/// The universal skill descriptor.
///
/// Contains all metadata about a skill regardless of where it comes from.
/// This is the single source of truth for what a skill *is*.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SkillManifest {
    /// Unique skill identifier.
    pub name: String,
    /// Semantic version.
    #[serde(default)]
    pub version: Version,
    /// Human-readable description.
    #[serde(default)]
    pub description: String,
    /// Skill author.
    #[serde(default)]
    pub author: Option<String>,
    /// Where this skill was loaded from.
    #[serde(default)]
    pub source: SkillSourceKind,
    /// Execution mode: inline (expand in conversation) or fork (sub-agent).
    #[serde(default)]
    pub execution_context: ExecutionContext,
    /// Whether users can manually invoke this skill.
    #[serde(default = "default_true")]
    pub user_invocable: bool,
    /// Keywords that trigger automatic skill selection.
    #[serde(default)]
    pub triggers: Vec<String>,
    /// Tools this skill is allowed to use (empty = all tools).
    #[serde(default)]
    pub allowed_tools: Vec<String>,
    /// Natural-language hint for when the model should pick this skill.
    #[serde(default)]
    pub when_to_use: Option<String>,
    /// Model override (e.g. `"claude-sonnet-4-20250514"`).
    #[serde(default)]
    pub model: Option<String>,
    /// Maximum token budget for a single invocation.
    #[serde(default)]
    pub max_tokens: Option<u32>,
    /// Lifecycle hooks (pre/post invoke, on error).
    #[serde(default)]
    pub hooks: Option<SkillHooks>,
    /// Glob patterns for conditional activation (skill only visible after matching files are touched).
    #[serde(default)]
    pub paths: Vec<String>,
    /// Named arguments that can be passed to the skill.
    #[serde(default)]
    pub arguments: Vec<SkillArgument>,
    /// Semver dependencies on other skills or tools.
    #[serde(default)]
    pub dependencies: Vec<Dependency>,
    /// Skill category (e.g. "code-review", "deployment", "analysis").
    #[serde(default)]
    pub category: Option<String>,
    /// Tags for discovery and search.
    #[serde(default)]
    pub tags: Vec<String>,
    /// Arbitrary additional metadata.
    #[serde(default)]
    pub metadata: HashMap<String, serde_json::Value>,

    // ── Capability fields (Phase 1) ─────────────────────────────────────────

    /// JSON Schema for structured skill input. Enables pre-execution validation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_schema: Option<serde_json::Value>,
    /// JSON Schema for structured skill output. Enables post-execution parsing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_schema: Option<serde_json::Value>,
    /// Machine-executable success criteria. Reuses durable-task verification types.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub success_criteria: Vec<VerificationCriterion>,
    /// Abstract capabilities this skill requires (e.g. "shell_execution", "file_read").
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub required_capabilities: Vec<String>,
    /// Composition metadata.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub composition: Option<SkillComposition>,
}

fn default_true() -> bool {
    true
}

/// Composition metadata for skill chaining.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SkillComposition {
    /// Whether this skill can be called by other skills.
    #[serde(default)]
    pub composable: bool,
    /// Whether running the skill twice produces the same result.
    #[serde(default)]
    pub idempotent: bool,
    /// What external state the skill may modify (e.g. "filesystem", "network").
    #[serde(default)]
    pub side_effects: Vec<String>,
    /// Maximum execution time in seconds (for orchestrators to enforce).
    #[serde(default)]
    pub max_duration_sec: Option<u32>,
}

/// Structured error categories for skill execution failures.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkillErrorKind {
    /// Input validation failed (bad arguments, missing required fields).
    InputValidation,
    /// Required tools or capabilities unavailable.
    MissingCapability,
    /// Skill execution timed out.
    Timeout,
    /// Verification criteria failed after execution.
    VerificationFailed,
    /// LLM produced unparseable or invalid output.
    OutputParsing,
    /// Internal/unexpected error.
    Internal,
}

impl Default for SkillManifest {
    fn default() -> Self {
        Self {
            name: String::new(),
            version: Version::default(),
            description: String::new(),
            author: None,
            source: SkillSourceKind::default(),
            execution_context: ExecutionContext::default(),
            user_invocable: true,
            triggers: Vec::new(),
            allowed_tools: Vec::new(),
            when_to_use: None,
            model: None,
            max_tokens: None,
            hooks: None,
            paths: Vec::new(),
            arguments: Vec::new(),
            dependencies: Vec::new(),
            category: None,
            tags: Vec::new(),
            metadata: HashMap::new(),
            input_schema: None,
            output_schema: None,
            success_criteria: Vec::new(),
            required_capabilities: Vec::new(),
            composition: None,
        }
    }
}

impl SkillManifest {
    /// Estimated token count for metadata (name + description + triggers + when_to_use).
    pub fn metadata_tokens(&self) -> u32 {
        let mut text = format!("{} {} {:?}", self.name, self.description, self.triggers);
        if let Some(ref wtu) = self.when_to_use {
            text.push(' ');
            text.push_str(wtu);
        }
        (text.len() as u32) / 4
    }

    /// Whether this skill requires isolated (fork) execution.
    pub fn is_isolated(&self) -> bool {
        self.execution_context == ExecutionContext::Fork
    }

    /// Whether this skill has path-based conditional activation.
    pub fn is_conditional(&self) -> bool {
        !self.paths.is_empty()
    }
}

/// A fully loaded skill: manifest + instruction text + optional resources.
#[derive(Clone, Debug)]
pub struct LoadedSkill {
    pub manifest: SkillManifest,
    /// Markdown instruction body (the content below SKILL.md frontmatter).
    pub instructions: String,
    /// Estimated tokens for instructions.
    pub instruction_tokens: u32,
    /// Level 3 resources: templates, scripts, external files.
    pub resources: Option<SkillResources>,
    /// Filesystem path to skill directory (if loaded from disk).
    pub skill_dir: Option<std::path::PathBuf>,
}

/// Level 3 resources: templates, scripts, and external files.
#[derive(Clone, Debug, Default)]
pub struct SkillResources {
    pub templates: HashMap<String, String>,
    pub scripts: HashMap<String, String>,
    pub resource_tokens: u32,
}

impl LoadedSkill {
    /// Total tokens loaded (manifest metadata + instructions + resources).
    pub fn total_tokens(&self) -> u32 {
        self.manifest.metadata_tokens()
            + self.instruction_tokens
            + self.resources.as_ref().map_or(0, |r| r.resource_tokens)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_default_values() {
        let m = SkillManifest::default();
        assert!(m.user_invocable);
        assert_eq!(m.execution_context, ExecutionContext::Inline);
        assert_eq!(m.source, SkillSourceKind::Local);
        assert!(!m.is_isolated());
        assert!(!m.is_conditional());
    }

    #[test]
    fn manifest_fork_detection() {
        let m = SkillManifest {
            execution_context: ExecutionContext::Fork,
            ..Default::default()
        };
        assert!(m.is_isolated());
    }

    #[test]
    fn manifest_conditional_detection() {
        let m = SkillManifest {
            paths: vec!["src/**/*.rs".into()],
            ..Default::default()
        };
        assert!(m.is_conditional());
    }

    #[test]
    fn manifest_metadata_tokens() {
        let m = SkillManifest {
            name: "test-skill".into(),
            description: "A test skill for demonstration".into(),
            triggers: vec!["test".into(), "demo".into()],
            when_to_use: Some("When testing the skill framework".into()),
            ..Default::default()
        };
        let tokens = m.metadata_tokens();
        assert!(tokens > 0);
        assert!(tokens < 100);
    }
}
