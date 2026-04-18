//! SKILL.md parser — loads skills from YAML frontmatter + Markdown body files.
//!
//! This is the canonical parser for the SKILL.md format used by local and bundled skills.
//! It supports all manifest fields including the new framework extensions (version,
//! execution_context, hooks, paths, arguments, dependencies).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use super::hooks::SkillHooks;
use super::manifest::{
    ExecutionContext, LoadedSkill, SkillManifest, SkillResources, SkillSourceKind,
};
use super::traits::SkillError;
use super::version::{Dependency, Version};

/// Intermediate deserialization target for SKILL.md YAML frontmatter.
///
/// Supports both legacy fields (from `skill_instructions.rs`) and new framework
/// extensions. All new fields are optional for backward compatibility.
#[derive(Debug, Clone, serde::Deserialize)]
struct RawFrontmatter {
    name: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    version: Option<String>,
    #[serde(default)]
    author: Option<String>,
    #[serde(default = "default_true")]
    user_invocable: bool,
    #[serde(default)]
    triggers: Vec<String>,
    #[serde(default)]
    allowed_tools: Vec<String>,
    #[serde(default)]
    when_to_use: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    max_tokens: Option<u32>,
    // New framework fields
    #[serde(default)]
    context: Option<String>,
    #[serde(default)]
    isolated: Option<bool>,
    #[serde(default)]
    hooks: Option<SkillHooks>,
    #[serde(default)]
    paths: Vec<String>,
    #[serde(default)]
    arguments: Vec<RawArgument>,
    #[serde(default)]
    depends_on: Vec<serde_yaml_ng::Value>,
    #[serde(default)]
    category: Option<String>,
    #[serde(default)]
    tags: Vec<String>,
    // Capability fields (Phase 1)
    #[serde(default)]
    input_schema: Option<serde_json::Value>,
    #[serde(default)]
    output_schema: Option<serde_json::Value>,
    #[serde(default)]
    success_criteria: Vec<serde_json::Value>,
    #[serde(default)]
    required_capabilities: Vec<String>,
    #[serde(default)]
    composition: Option<super::manifest::SkillComposition>,
    // Marketplace fields (Phase 3)
    #[serde(default)]
    trust_tier: Option<String>,
    #[serde(default)]
    publisher: Option<super::manifest::PublisherMetadata>,
    #[serde(default)]
    compatibility: Option<super::manifest::CompatibilityInfo>,
    // CC-compatible fields
    #[serde(default)]
    aliases: Vec<String>,
    #[serde(default)]
    effort: Option<String>,
    #[serde(default)]
    agent_type: Option<String>,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, serde::Deserialize)]
struct RawArgument {
    name: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    required: bool,
    #[serde(default)]
    default: Option<String>,
}

/// Parse a SKILL.md content string into a `SkillManifest` and instruction body.
///
/// Returns `(manifest, instructions_body)`.
pub fn parse_skill_md(content: &str) -> Result<(SkillManifest, String), SkillError> {
    let content = content.trim();

    if !content.starts_with("---") {
        return Err(SkillError::ParseFailed(
            "SKILL.md must start with YAML frontmatter (---)".into(),
        ));
    }

    let rest = &content[3..];
    let end_marker = rest.find("\n---").ok_or_else(|| {
        SkillError::ParseFailed("Missing closing frontmatter marker (---)".into())
    })?;

    let yaml_content = rest[..end_marker].trim();
    let markdown_body = rest[end_marker + 4..].trim().to_string();

    let raw: RawFrontmatter = serde_yaml_ng::from_str(yaml_content)
        .map_err(|e| SkillError::ParseFailed(format!("Failed to parse YAML frontmatter: {e}")))?;

    validate_skill_name(&raw.name)?;

    let version = raw
        .version
        .map(|v| v.parse::<Version>())
        .transpose()
        .map_err(|e| SkillError::ParseFailed(format!("Invalid version: {e}")))?
        .unwrap_or_default();

    let execution_context = match (raw.context.as_deref(), raw.isolated) {
        (Some("fork"), _) | (_, Some(true)) => ExecutionContext::Fork,
        _ => ExecutionContext::Inline,
    };

    let arguments = raw
        .arguments
        .into_iter()
        .map(|a| super::manifest::SkillArgument {
            name: a.name,
            description: a.description,
            required: a.required,
            default: a.default,
        })
        .collect();

    let dependencies = parse_dependencies(&raw.depends_on)?;

    let manifest = SkillManifest {
        name: raw.name,
        version,
        description: raw.description,
        author: raw.author,
        source: SkillSourceKind::Local,
        execution_context,
        user_invocable: raw.user_invocable,
        triggers: raw.triggers,
        allowed_tools: raw.allowed_tools,
        when_to_use: raw.when_to_use,
        model: raw.model,
        max_tokens: raw.max_tokens,
        hooks: raw.hooks,
        paths: raw.paths,
        arguments,
        dependencies,
        category: raw.category,
        tags: raw.tags,
        metadata: HashMap::new(),
        input_schema: raw.input_schema,
        output_schema: raw.output_schema,
        remote_url: None,
        forward_headers: Vec::new(),
        required_headers: Vec::new(),
        success_criteria: raw.success_criteria,
        required_capabilities: raw.required_capabilities,
        composition: raw.composition,
        trust_tier: match raw.trust_tier.as_deref() {
            Some("bundled") => super::manifest::TrustTier::Bundled,
            Some("verified") => super::manifest::TrustTier::Verified,
            Some("community") => super::manifest::TrustTier::Community,
            _ => super::manifest::TrustTier::Unverified,
        },
        publisher: raw.publisher,
        compatibility: raw.compatibility,
        aliases: raw.aliases,
        effort: raw
            .effort
            .as_deref()
            .and_then(super::manifest::EffortLevel::parse),
        agent_type: raw.agent_type,
    };

    Ok((manifest, markdown_body))
}

/// Parse the `depends_on` field which supports both old format (list of strings)
/// and new format (list of {name, version, type} objects).
fn parse_dependencies(raw: &[serde_yaml_ng::Value]) -> Result<Vec<Dependency>, SkillError> {
    let mut deps = Vec::new();
    for item in raw {
        match item {
            serde_yaml_ng::Value::String(name) => {
                deps.push(Dependency {
                    name: name.clone(),
                    version: super::version::VersionConstraint::any(),
                    dep_type: super::version::DependencyType::Skill,
                });
            }
            serde_yaml_ng::Value::Mapping(map) => {
                let name = map
                    .get(serde_yaml_ng::Value::String("name".into()))
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| {
                        SkillError::ParseFailed("dependency missing 'name' field".into())
                    })?
                    .to_string();

                let version = map
                    .get(serde_yaml_ng::Value::String("version".into()))
                    .and_then(|v| v.as_str())
                    .unwrap_or("*")
                    .parse()
                    .map_err(|e: String| SkillError::ParseFailed(e))?;

                let dep_type = match map
                    .get(serde_yaml_ng::Value::String("type".into()))
                    .and_then(|v| v.as_str())
                {
                    Some("tool") => super::version::DependencyType::Tool,
                    _ => super::version::DependencyType::Skill,
                };

                deps.push(Dependency {
                    name,
                    version,
                    dep_type,
                });
            }
            _ => {
                return Err(SkillError::ParseFailed(
                    "depends_on entries must be strings or objects".into(),
                ));
            }
        }
    }
    Ok(deps)
}

/// Load a SKILL.md file from disk into a `LoadedSkill`.
///
/// Applies `..` path traversal checks only. For symlink confinement, use
/// [`load_skill_from_path_confined`] which additionally verifies the
/// canonical path stays within the given root directory.
pub fn load_skill_from_path(path: &Path) -> Result<LoadedSkill, SkillError> {
    reject_path_traversal(path)?;
    load_skill_from_path_inner(path)
}

/// Load a SKILL.md file, verifying the canonical path is confined to `root`.
///
/// Blocks both `..` traversal and symlink escapes. Use this when loading
/// skills from untrusted directory trees (e.g. local skill search paths).
pub fn load_skill_from_path_confined(path: &Path, root: &Path) -> Result<LoadedSkill, SkillError> {
    reject_path_traversal(path)?;
    verify_confinement(path, root)?;
    load_skill_from_path_inner(path)
}

fn load_skill_from_path_inner(path: &Path) -> Result<LoadedSkill, SkillError> {
    let content = std::fs::read_to_string(path)
        .map_err(|e| SkillError::LoadFailed(format!("Failed to read {}: {e}", path.display())))?;

    let (mut manifest, instructions) = parse_skill_md(&content)?;
    manifest.source = SkillSourceKind::Local;

    let instruction_tokens = (instructions.len() as u32) / 4;

    let skill_dir = path.parent().map(|p| p.to_path_buf());

    Ok(LoadedSkill {
        manifest,
        instructions,
        instruction_tokens,
        resources: None,
        skill_dir,
    })
}

/// Reject paths that contain traversal components (`..`).
fn reject_path_traversal(path: &Path) -> Result<(), SkillError> {
    for component in path.components() {
        if let std::path::Component::ParentDir = component {
            return Err(SkillError::PermissionDenied(format!(
                "path traversal blocked (..): {}",
                path.display()
            )));
        }
    }
    Ok(())
}

/// Validate that a skill name is safe for use in file paths and identifiers.
///
/// Rejects names containing path separators (`/`, `\`), traversal (`..`),
/// null bytes, or control characters. Names must also be non-empty and
/// not exceed 128 characters.
fn validate_skill_name(name: &str) -> Result<(), SkillError> {
    if name.is_empty() {
        return Err(SkillError::ParseFailed("skill name cannot be empty".into()));
    }
    if name.len() > 128 {
        return Err(SkillError::ParseFailed(format!(
            "skill name too long ({} chars, max 128): {}",
            name.len(),
            &name[..name.floor_char_boundary(32)]
        )));
    }
    if name.contains('/') || name.contains('\\') || name.contains('\0') {
        return Err(SkillError::PermissionDenied(format!(
            "skill name contains path separator or null: {name:?}"
        )));
    }
    if name.contains("..") {
        return Err(SkillError::PermissionDenied(format!(
            "skill name contains path traversal: {name:?}"
        )));
    }
    if name.chars().any(|c| c.is_control()) {
        return Err(SkillError::PermissionDenied(format!(
            "skill name contains control characters: {name:?}"
        )));
    }
    Ok(())
}

/// Sanitize a string for safe use as a filesystem path component.
///
/// Replaces any character that could cause path injection with `-`.
pub fn sanitize_for_path(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            '/' | '\\' | '\0' | ':' => '-',
            '.' if s.contains("..") => '-',
            c if c.is_control() => '-',
            _ => c,
        })
        .collect()
}

/// Verify that `path` resolves (via canonicalize) to a location within `root`.
///
/// Catches symlink-based escapes: if `.astra/skills/evil` is a symlink
/// pointing outside the search directory, the canonical path won't be a
/// descendant of `root`'s canonical path.
fn verify_confinement(path: &Path, root: &Path) -> Result<(), SkillError> {
    let canonical_path = std::fs::canonicalize(path).map_err(|e| {
        SkillError::LoadFailed(format!("cannot canonicalize {}: {e}", path.display()))
    })?;
    let canonical_root = std::fs::canonicalize(root).map_err(|e| {
        SkillError::LoadFailed(format!("cannot canonicalize root {}: {e}", root.display()))
    })?;
    if !canonical_path.starts_with(&canonical_root) {
        return Err(SkillError::PermissionDenied(format!(
            "symlink escape blocked: {} resolves to {} which is outside {}",
            path.display(),
            canonical_path.display(),
            canonical_root.display(),
        )));
    }
    Ok(())
}

/// Load Level 3 resources (templates/ and scripts/ subdirectories) for a skill.
pub fn load_skill_resources(skill_dir: &Path) -> Result<SkillResources, SkillError> {
    let mut resources = SkillResources::default();

    let templates_dir = skill_dir.join("templates");
    if templates_dir.exists() {
        load_dir_contents(
            &templates_dir,
            &mut resources.templates,
            &mut resources.resource_tokens,
        )?;
    }

    let scripts_dir = skill_dir.join("scripts");
    if scripts_dir.exists() {
        load_dir_contents(
            &scripts_dir,
            &mut resources.scripts,
            &mut resources.resource_tokens,
        )?;
    }

    Ok(resources)
}

fn load_dir_contents(
    dir: &Path,
    target: &mut HashMap<String, String>,
    tokens: &mut u32,
) -> Result<(), SkillError> {
    let canonical_dir = std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());

    let entries = std::fs::read_dir(dir)
        .map_err(|e| SkillError::LoadFailed(format!("Failed to read {}: {e}", dir.display())))?;

    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_file() {
            // Ensure the file doesn't escape the resource directory via symlinks.
            if let Ok(canonical) = std::fs::canonicalize(&path)
                && !canonical.starts_with(&canonical_dir)
            {
                continue; // symlink escape — skip silently
            }
            if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                let content = std::fs::read_to_string(&path).map_err(|e| {
                    SkillError::LoadFailed(format!("Failed to read {}: {e}", path.display()))
                })?;
                *tokens += (content.len() as u32) / 4;
                target.insert(name.to_string(), content);
            }
        }
    }
    Ok(())
}

/// Discover SKILL.md files in a directory (non-recursive, looks for `{name}/SKILL.md`).
///
/// Returns pairs of `(skill_name, skill_md_path)`.
///
/// Security: canonicalizes both the search directory and each discovered skill
/// entry. Entries whose canonical path escapes the search directory (via symlinks)
/// are silently skipped. Duplicate canonical paths are also deduplicated.
pub fn discover_skills_in_dir(dir: &Path) -> Vec<(String, PathBuf)> {
    let mut skills = Vec::new();
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return skills,
    };

    let canonical_dir = match std::fs::canonicalize(dir) {
        Ok(c) => c,
        Err(_) => return skills,
    };

    let mut seen_canonical: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();

    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            let skill_md = path.join("SKILL.md");
            if skill_md.exists() {
                if let Ok(canonical) = std::fs::canonicalize(&skill_md) {
                    // Containment: reject if the real path escapes the search dir.
                    if !canonical.starts_with(&canonical_dir) {
                        continue;
                    }
                    // Dedup: skip if we've already seen this canonical path.
                    if !seen_canonical.insert(canonical) {
                        continue;
                    }
                } else {
                    // Cannot resolve canonical path — skip for safety.
                    continue;
                }
                if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                    skills.push((name.to_string(), skill_md));
                }
            }
        }
    }
    skills
}

/// Standard skill directory search order (high -> low priority):
///
/// 1. Walk-up from cwd: `{ancestor}/.astra/skills/` for each ancestor
/// 2. Walk-up from cwd: `{ancestor}/.claude/skills/` for each ancestor (CC-compatible)
/// 3. `{cwd}/skills/`         — project-level (legacy)
/// 4. `~/.astra/skills/`      — user-level global skills
/// 5. `~/.claude/skills/`     — Claude Code user-level skills (CC-compatible)
///
/// Walk-up discovery traverses from `cwd` upward to the filesystem root,
/// collecting skill directories. Astra's SKILL.md format is compatible with
/// the Agent Skills open standard used by Claude Code, so skills authored for
/// either tool work in both. Claude Code skills are discovered at lower
/// priority so astra-native skills take precedence when names collide.
pub fn skill_search_paths() -> Vec<PathBuf> {
    let mut paths = Vec::with_capacity(12);

    if let Ok(cwd) = std::env::current_dir() {
        // Single walk-up collecting both .astra/skills/ and .claude/skills/.
        // .astra paths first at each level so they take priority over .claude.
        let (astra, claude) = walk_up_skill_paths(&cwd);
        paths.extend(astra);
        paths.extend(claude);
        // Legacy: skills/ in cwd only (not walked up).
        paths.push(cwd.join("skills"));
    } else {
        paths.push(PathBuf::from(".astra/skills"));
        paths.push(PathBuf::from(".claude/skills"));
        paths.push(PathBuf::from("skills"));
    }

    if let Some(home) = dirs::home_dir() {
        let global = home.join(".astra").join("skills");
        if !paths.contains(&global) {
            paths.push(global);
        }
        let cc_global = home.join(".claude").join("skills");
        if !paths.contains(&cc_global) {
            paths.push(cc_global);
        }
    }

    paths
}

/// Walk from `start` upward once, collecting both `.astra/skills/` and
/// `.claude/skills/` at each ancestor. Returns `(astra_paths, claude_paths)`
/// so the caller can maintain priority (astra before claude).
///
/// Stops at repository root (`.git`) or user's home directory.
pub fn walk_up_skill_paths(start: &Path) -> (Vec<PathBuf>, Vec<PathBuf>) {
    let mut astra = Vec::new();
    let mut claude = Vec::new();
    let mut dir = start.to_path_buf();
    let home = dirs::home_dir();

    loop {
        let a = dir.join(".astra").join("skills");
        if a.is_dir() {
            astra.push(a);
        }
        let c = dir.join(".claude").join("skills");
        if c.is_dir() {
            claude.push(c);
        }

        if dir.join(".git").exists() {
            break;
        }
        if matches!(&home, Some(h) if dir == *h) {
            break;
        }
        if !dir.pop() {
            break;
        }
    }

    (astra, claude)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::EffortLevel;

    #[test]
    fn parse_basic_skill() {
        let content = r#"---
name: test-skill
description: "A test skill"
triggers:
  - test
  - demo
allowed_tools:
  - bash
---
# Instructions

Step 1: Do the thing.
Step 2: Done.
"#;
        let (manifest, instructions) = parse_skill_md(content).unwrap();
        assert_eq!(manifest.name, "test-skill");
        assert_eq!(manifest.description, "A test skill");
        assert_eq!(manifest.triggers, vec!["test", "demo"]);
        assert_eq!(manifest.allowed_tools, vec!["bash"]);
        assert!(manifest.user_invocable);
        assert_eq!(manifest.execution_context, ExecutionContext::Inline);
        assert!(instructions.contains("Step 1"));
    }

    #[test]
    fn parse_full_manifest() {
        let content = r#"---
name: deep-review
description: "Thorough multi-file code review"
version: "2.1.0"
author: "astra-team"
user_invocable: true
context: fork
model: "claude-sonnet-4-20250514"
max_tokens: 16384
when_to_use: "Use for thorough code reviews"
triggers:
  - review
  - code-review
allowed_tools:
  - bash
  - read_file
  - grep
paths:
  - "src/**/*.rs"
  - "tests/**/*.rs"
arguments:
  - name: file
    description: "File to review"
    required: true
depends_on:
  - name: git_status
    version: ">=1.0"
    type: tool
  - name: knowledge
    version: "~=2.1.0"
    type: skill
category: code-review
tags:
  - review
  - security
---
# Deep Review Instructions

Analyze the code thoroughly.
"#;
        let (manifest, instructions) = parse_skill_md(content).unwrap();
        assert_eq!(manifest.name, "deep-review");
        assert_eq!(manifest.version.major, 2);
        assert_eq!(manifest.version.minor, 1);
        assert_eq!(manifest.version.patch, 0);
        assert_eq!(manifest.author.as_deref(), Some("astra-team"));
        assert_eq!(manifest.execution_context, ExecutionContext::Fork);
        assert_eq!(manifest.model.as_deref(), Some("claude-sonnet-4-20250514"));
        assert_eq!(manifest.max_tokens, Some(16384));
        assert!(manifest.is_isolated());
        assert!(manifest.is_conditional());
        assert_eq!(manifest.paths.len(), 2);
        assert_eq!(manifest.arguments.len(), 1);
        assert!(manifest.arguments[0].required);
        assert_eq!(manifest.dependencies.len(), 2);
        assert_eq!(manifest.category.as_deref(), Some("code-review"));
        assert_eq!(manifest.tags, vec!["review", "security"]);
        assert!(instructions.contains("Analyze the code thoroughly"));
    }

    #[test]
    fn parse_isolated_flag() {
        let content = r#"---
name: isolated-skill
description: "Uses isolated flag"
isolated: true
---
Run in isolation.
"#;
        let (manifest, _) = parse_skill_md(content).unwrap();
        assert_eq!(manifest.execution_context, ExecutionContext::Fork);
        assert!(manifest.is_isolated());
    }

    #[test]
    fn parse_legacy_depends_on() {
        let content = r#"---
name: legacy
description: "Legacy deps"
depends_on:
  - github
  - jira
---
Instructions.
"#;
        let (manifest, _) = parse_skill_md(content).unwrap();
        assert_eq!(manifest.dependencies.len(), 2);
        assert_eq!(manifest.dependencies[0].name, "github");
        assert!(
            manifest.dependencies[0]
                .version
                .matches(&Version::new(99, 0, 0))
        );
    }

    #[test]
    fn parse_missing_frontmatter() {
        let result = parse_skill_md("Just markdown, no frontmatter");
        assert!(matches!(result, Err(SkillError::ParseFailed(_))));
    }

    #[test]
    fn parse_unclosed_frontmatter() {
        let content = "---\nname: broken\n";
        let result = parse_skill_md(content);
        assert!(matches!(result, Err(SkillError::ParseFailed(_))));
    }

    #[test]
    fn discover_skills_finds_directories() {
        let dir = tempfile::TempDir::new().unwrap();
        let skill_dir = dir.path().join("my-skill");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: my-skill\ndescription: test\n---\nBody",
        )
        .unwrap();

        let found = discover_skills_in_dir(dir.path());
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].0, "my-skill");
    }

    #[test]
    fn load_resources_from_dir() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join("templates")).unwrap();
        std::fs::create_dir_all(dir.path().join("scripts")).unwrap();
        std::fs::write(dir.path().join("templates/config.yaml"), "key: value").unwrap();
        std::fs::write(dir.path().join("scripts/setup.sh"), "#!/bin/bash\necho ok").unwrap();

        let resources = load_skill_resources(dir.path()).unwrap();
        assert!(resources.templates.contains_key("config.yaml"));
        assert!(resources.scripts.contains_key("setup.sh"));
        assert!(resources.resource_tokens > 0);
    }

    // ── Additional edge case tests ───────────────────────────────────────

    #[test]
    fn parse_empty_body_after_frontmatter() {
        let content = "---\nname: empty-body\ndescription: test\n---\n";
        let (manifest, instructions) = parse_skill_md(content).unwrap();
        assert_eq!(manifest.name, "empty-body");
        assert!(instructions.is_empty());
    }

    #[test]
    fn parse_frontmatter_with_leading_whitespace_in_content() {
        let content = "---\nname: spaced\ndescription: \"description with spaces\"\n---\n\n  Body here with leading spaces.";
        let (manifest, instructions) = parse_skill_md(content).unwrap();
        assert_eq!(manifest.name, "spaced");
        assert!(instructions.contains("Body here with leading spaces"));
    }

    #[test]
    fn parse_invalid_yaml_in_frontmatter() {
        let content = "---\n[invalid yaml\n---\nBody";
        let result = parse_skill_md(content);
        assert!(matches!(result, Err(SkillError::ParseFailed(_))));
    }

    #[test]
    fn parse_missing_name_field() {
        let content = "---\ndescription: no name\n---\nBody";
        let result = parse_skill_md(content);
        assert!(matches!(result, Err(SkillError::ParseFailed(_))));
    }

    #[test]
    fn parse_invalid_version_string() {
        let content = "---\nname: bad-ver\nversion: not-semver\n---\nBody";
        let result = parse_skill_md(content);
        assert!(matches!(result, Err(SkillError::ParseFailed(_))));
    }

    #[test]
    fn parse_context_fork_sets_execution_context() {
        let content = "---\nname: fork-skill\ncontext: fork\n---\nFork me.";
        let (manifest, _) = parse_skill_md(content).unwrap();
        assert_eq!(manifest.execution_context, ExecutionContext::Fork);
    }

    #[test]
    fn parse_inline_is_default_execution_context() {
        let content = "---\nname: inline-skill\n---\nInline.";
        let (manifest, _) = parse_skill_md(content).unwrap();
        assert_eq!(manifest.execution_context, ExecutionContext::Inline);
    }

    #[test]
    fn parse_user_invocable_defaults_true() {
        let content = "---\nname: invocable\n---\nBody.";
        let (manifest, _) = parse_skill_md(content).unwrap();
        assert!(manifest.user_invocable);
    }

    #[test]
    fn parse_user_invocable_false() {
        let content = "---\nname: hidden\nuser_invocable: false\n---\nBody.";
        let (manifest, _) = parse_skill_md(content).unwrap();
        assert!(!manifest.user_invocable);
    }

    #[test]
    fn parse_dependency_missing_name_errors() {
        let content = "---\nname: bad-dep\ndepends_on:\n  - version: \">=1.0\"\n---\nBody";
        let result = parse_skill_md(content);
        assert!(matches!(result, Err(SkillError::ParseFailed(_))));
    }

    #[test]
    fn parse_dependency_invalid_type_defaults_to_skill() {
        let content = "---\nname: dep-type\ndepends_on:\n  - name: foo\n    version: \">=1.0\"\n    type: unknown\n---\nBody";
        let (manifest, _) = parse_skill_md(content).unwrap();
        assert_eq!(
            manifest.dependencies[0].dep_type,
            super::super::version::DependencyType::Skill
        );
    }

    #[test]
    fn parse_arguments_with_defaults() {
        let content = r#"---
name: args-test
arguments:
  - name: FILE
    description: "Target file"
    required: true
  - name: MODE
    description: "Mode"
    required: false
    default: "fast"
---
Body."#;
        let (manifest, _) = parse_skill_md(content).unwrap();
        assert_eq!(manifest.arguments.len(), 2);
        assert!(manifest.arguments[0].required);
        assert!(!manifest.arguments[1].required);
        assert_eq!(manifest.arguments[1].default.as_deref(), Some("fast"));
    }

    #[test]
    fn parse_hooks_from_frontmatter() {
        let content = r#"---
name: hooked
hooks:
  pre_invoke:
    - type: shell
      command: "echo before"
  post_invoke:
    - type: set_env
      key: DONE
      value: "1"
---
Hooked body."#;
        let (manifest, _) = parse_skill_md(content).unwrap();
        let hooks = manifest.hooks.unwrap();
        assert_eq!(hooks.pre_invoke.len(), 1);
        assert_eq!(hooks.post_invoke.len(), 1);
        assert!(hooks.on_error.is_empty());
    }

    #[test]
    fn load_skill_from_path_success() {
        let dir = tempfile::TempDir::new().unwrap();
        let md_path = dir.path().join("SKILL.md");
        std::fs::write(
            &md_path,
            "---\nname: from-path\ndescription: loaded\n---\nInstructions here.",
        )
        .unwrap();

        let loaded = load_skill_from_path(&md_path).unwrap();
        assert_eq!(loaded.manifest.name, "from-path");
        assert_eq!(loaded.manifest.source, SkillSourceKind::Local);
        assert!(loaded.instructions.contains("Instructions here."));
        assert!(loaded.skill_dir.is_some());
    }

    #[test]
    fn load_skill_from_nonexistent_path_errors() {
        let result = load_skill_from_path(Path::new("/tmp/nonexistent/SKILL.md"));
        assert!(matches!(result, Err(SkillError::LoadFailed(_))));
    }

    #[test]
    fn discover_empty_dir_returns_empty() {
        let dir = tempfile::TempDir::new().unwrap();
        let found = discover_skills_in_dir(dir.path());
        assert!(found.is_empty());
    }

    #[test]
    fn discover_nonexistent_dir_returns_empty() {
        let found = discover_skills_in_dir(Path::new("/tmp/does_not_exist_12345"));
        assert!(found.is_empty());
    }

    #[test]
    fn discover_skips_dirs_without_skill_md() {
        let dir = tempfile::TempDir::new().unwrap();
        let sub = dir.path().join("no-skill");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(sub.join("README.md"), "Not a skill").unwrap();

        let found = discover_skills_in_dir(dir.path());
        assert!(found.is_empty());
    }

    #[test]
    fn load_resources_no_subdirs() {
        let dir = tempfile::TempDir::new().unwrap();
        let resources = load_skill_resources(dir.path()).unwrap();
        assert!(resources.templates.is_empty());
        assert!(resources.scripts.is_empty());
        assert_eq!(resources.resource_tokens, 0);
    }

    #[test]
    fn skill_search_paths_returns_at_least_two() {
        let paths = skill_search_paths();
        assert!(paths.len() >= 2);
    }

    #[test]
    fn skill_search_paths_includes_claude_dirs() {
        let paths = skill_search_paths();
        let has_claude = paths
            .iter()
            .any(|p| p.components().any(|c| c.as_os_str() == ".claude"));
        assert!(
            has_claude,
            "should include .claude/skills/ paths, got: {paths:?}"
        );
    }

    // ── Path traversal protection ──

    #[test]
    fn reject_path_traversal_blocks_dotdot() {
        let path = Path::new("/tmp/skills/../../../etc/passwd");
        let result = reject_path_traversal(path);
        assert!(matches!(result, Err(SkillError::PermissionDenied(_))));
    }

    #[test]
    fn reject_path_traversal_allows_normal_paths() {
        let path = Path::new("/tmp/skills/review/SKILL.md");
        assert!(reject_path_traversal(path).is_ok());
    }

    #[test]
    fn load_skill_from_path_rejects_traversal() {
        let result = load_skill_from_path(Path::new("/tmp/../../etc/SKILL.md"));
        assert!(matches!(result, Err(SkillError::PermissionDenied(_))));
    }

    // ── Symlink containment & dedup in discovery ──

    #[cfg(unix)]
    #[test]
    fn discover_deduplicates_symlinks() {
        let dir = tempfile::TempDir::new().unwrap();
        let real_skill = dir.path().join("real-skill");
        std::fs::create_dir_all(&real_skill).unwrap();
        std::fs::write(
            real_skill.join("SKILL.md"),
            "---\nname: dedup-test\ndescription: test\n---\nInstructions.",
        )
        .unwrap();

        let symlink_skill = dir.path().join("linked-skill");
        std::os::unix::fs::symlink(&real_skill, &symlink_skill).unwrap();

        let found = discover_skills_in_dir(dir.path());
        assert_eq!(found.len(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn discover_rejects_symlink_escape() {
        let search_dir = tempfile::TempDir::new().unwrap();
        let external_dir = tempfile::TempDir::new().unwrap();

        let evil_skill = external_dir.path().join("evil-skill");
        std::fs::create_dir_all(&evil_skill).unwrap();
        std::fs::write(
            evil_skill.join("SKILL.md"),
            "---\nname: evil\ndescription: pwned\n---\nrm -rf /",
        )
        .unwrap();

        // Symlink from search_dir/evil -> external target
        let link_in_search = search_dir.path().join("evil");
        std::os::unix::fs::symlink(&evil_skill, &link_in_search).unwrap();

        let found = discover_skills_in_dir(search_dir.path());
        assert!(
            found.is_empty(),
            "symlink escaping the search directory should be rejected"
        );
    }

    #[cfg(unix)]
    #[test]
    fn load_confined_rejects_symlink_escape() {
        let search_dir = tempfile::TempDir::new().unwrap();
        let external_dir = tempfile::TempDir::new().unwrap();

        let real_skill = external_dir.path().join("escape-test");
        std::fs::create_dir_all(&real_skill).unwrap();
        std::fs::write(
            real_skill.join("SKILL.md"),
            "---\nname: escape\ndescription: test\n---\nInstructions.",
        )
        .unwrap();

        let link = search_dir.path().join("escape-test");
        std::os::unix::fs::symlink(&real_skill, &link).unwrap();

        let skill_md = link.join("SKILL.md");
        let result = load_skill_from_path_confined(&skill_md, search_dir.path());
        assert!(
            matches!(result, Err(SkillError::PermissionDenied(_))),
            "confined load should reject symlink escape, got: {result:?}"
        );
    }

    #[test]
    fn load_confined_allows_normal_paths() {
        let dir = tempfile::TempDir::new().unwrap();
        let skill_dir = dir.path().join("good-skill");
        std::fs::create_dir_all(&skill_dir).unwrap();
        let md_path = skill_dir.join("SKILL.md");
        std::fs::write(
            &md_path,
            "---\nname: good\ndescription: test\n---\nInstructions.",
        )
        .unwrap();

        let loaded = load_skill_from_path_confined(&md_path, dir.path()).unwrap();
        assert_eq!(loaded.manifest.name, "good");
    }

    #[test]
    fn verify_confinement_rejects_outside_root() {
        let root = tempfile::TempDir::new().unwrap();
        let outside = tempfile::TempDir::new().unwrap();
        let file = outside.path().join("test.txt");
        std::fs::write(&file, "hello").unwrap();

        let result = verify_confinement(&file, root.path());
        assert!(matches!(result, Err(SkillError::PermissionDenied(_))));
    }

    #[test]
    fn verify_confinement_allows_inside_root() {
        let root = tempfile::TempDir::new().unwrap();
        let file = root.path().join("inside.txt");
        std::fs::write(&file, "hello").unwrap();

        assert!(verify_confinement(&file, root.path()).is_ok());
    }

    // ── Walk-up discovery ──

    #[test]
    fn walk_up_finds_skill_dirs() {
        let dir = tempfile::TempDir::new().unwrap();
        let deep = dir.path().join("a").join("b").join("c");
        std::fs::create_dir_all(&deep).unwrap();

        // Create .git at project root to define trust boundary
        std::fs::create_dir_all(dir.path().join(".git")).unwrap();

        // Create .astra/skills at the root
        let root_skills = dir.path().join(".astra").join("skills");
        std::fs::create_dir_all(&root_skills).unwrap();

        // Create .astra/skills at level "a"
        let mid_skills = dir.path().join("a").join(".astra").join("skills");
        std::fs::create_dir_all(&mid_skills).unwrap();

        let (astra, _claude) = walk_up_skill_paths(&deep);
        assert_eq!(astra.len(), 2);
        assert_eq!(astra[0], mid_skills);
        assert_eq!(astra[1], root_skills);
    }

    #[test]
    fn walk_up_stops_at_git_boundary() {
        let dir = tempfile::TempDir::new().unwrap();
        let repo = dir.path().join("repo");
        let deep = repo.join("src").join("pkg");
        std::fs::create_dir_all(&deep).unwrap();
        std::fs::create_dir_all(repo.join(".git")).unwrap();

        // Skills inside the repo — should be found
        let repo_skills = repo.join(".astra").join("skills");
        std::fs::create_dir_all(&repo_skills).unwrap();

        // Skills OUTSIDE the repo (in parent) — should NOT be found
        let outside_skills = dir.path().join(".astra").join("skills");
        std::fs::create_dir_all(&outside_skills).unwrap();

        let (astra, _claude) = walk_up_skill_paths(&deep);

        assert!(
            astra.contains(&repo_skills),
            "should find skills inside repo"
        );
        assert!(
            !astra.contains(&outside_skills),
            "should NOT find skills outside repo root"
        );
    }

    #[test]
    fn walk_up_returns_empty_when_no_skills() {
        let dir = tempfile::TempDir::new().unwrap();
        let deep = dir.path().join("no").join("skills").join("here");
        std::fs::create_dir_all(&deep).unwrap();
        // Place .git so we don't walk outside the temp dir
        std::fs::create_dir_all(dir.path().join(".git")).unwrap();

        let (astra, claude) = walk_up_skill_paths(&deep);
        assert!(astra.is_empty());
        assert!(claude.is_empty());
    }

    #[test]
    fn walk_up_finds_claude_skills() {
        let dir = tempfile::TempDir::new().unwrap();
        let deep = dir.path().join("src");
        std::fs::create_dir_all(&deep).unwrap();
        std::fs::create_dir_all(dir.path().join(".git")).unwrap();

        let cc_skills = dir.path().join(".claude").join("skills");
        std::fs::create_dir_all(&cc_skills).unwrap();

        let (astra, claude) = walk_up_skill_paths(&deep);
        assert!(astra.is_empty(), "no .astra/skills/ exists");
        assert_eq!(claude.len(), 1);
        assert_eq!(claude[0], cc_skills);
    }

    #[test]
    fn walk_up_finds_both_astra_and_claude() {
        let dir = tempfile::TempDir::new().unwrap();
        let deep = dir.path().join("pkg").join("sub");
        std::fs::create_dir_all(&deep).unwrap();
        std::fs::create_dir_all(dir.path().join(".git")).unwrap();

        let astra_root = dir.path().join(".astra").join("skills");
        let claude_root = dir.path().join(".claude").join("skills");
        let claude_mid = dir.path().join("pkg").join(".claude").join("skills");
        std::fs::create_dir_all(&astra_root).unwrap();
        std::fs::create_dir_all(&claude_root).unwrap();
        std::fs::create_dir_all(&claude_mid).unwrap();

        let (astra, claude) = walk_up_skill_paths(&deep);
        assert_eq!(astra, vec![astra_root]);
        assert_eq!(claude.len(), 2);
        assert_eq!(claude[0], claude_mid, "deeper .claude first");
        assert_eq!(claude[1], claude_root);
    }

    #[test]
    fn walk_up_claude_stops_at_git_boundary() {
        let dir = tempfile::TempDir::new().unwrap();
        let repo = dir.path().join("repo");
        let deep = repo.join("src");
        std::fs::create_dir_all(&deep).unwrap();
        std::fs::create_dir_all(repo.join(".git")).unwrap();

        // Inside repo — should be found
        let inside = repo.join(".claude").join("skills");
        std::fs::create_dir_all(&inside).unwrap();

        // Outside repo — should NOT be found
        let outside = dir.path().join(".claude").join("skills");
        std::fs::create_dir_all(&outside).unwrap();

        let (_astra, claude) = walk_up_skill_paths(&deep);
        assert!(claude.contains(&inside));
        assert!(
            !claude.contains(&outside),
            ".claude outside git root must be excluded"
        );
    }

    // ── Skill name validation ──

    #[test]
    fn validate_skill_name_rejects_slash() {
        let content = "---\nname: evil/skill\ndescription: test\n---\nBody";
        let result = parse_skill_md(content);
        assert!(matches!(result, Err(SkillError::PermissionDenied(_))));
    }

    #[test]
    fn validate_skill_name_rejects_dotdot() {
        let content = "---\nname: ..escape\ndescription: test\n---\nBody";
        let result = parse_skill_md(content);
        assert!(matches!(result, Err(SkillError::PermissionDenied(_))));
    }

    #[test]
    fn validate_skill_name_rejects_backslash() {
        let content = "---\nname: \"evil\\\\skill\"\ndescription: test\n---\nBody";
        let result = parse_skill_md(content);
        assert!(matches!(result, Err(SkillError::PermissionDenied(_))));
    }

    #[test]
    fn validate_skill_name_rejects_null() {
        let content = "---\nname: \"evil\\0skill\"\ndescription: test\n---\nBody";
        let result = parse_skill_md(content);
        assert!(matches!(result, Err(SkillError::PermissionDenied(_))));
    }

    #[test]
    fn validate_skill_name_allows_normal() {
        let content = "---\nname: my-cool-skill_v2\ndescription: test\n---\nBody";
        let (manifest, _) = parse_skill_md(content).unwrap();
        assert_eq!(manifest.name, "my-cool-skill_v2");
    }

    #[test]
    fn validate_skill_name_rejects_empty() {
        let content = "---\nname: \"\"\ndescription: test\n---\nBody";
        let result = parse_skill_md(content);
        assert!(result.is_err());
    }

    // ── sanitize_for_path ──

    #[test]
    fn sanitize_path_replaces_slash_and_dotdot() {
        assert_eq!(sanitize_for_path("evil/skill"), "evil-skill");
        assert_eq!(sanitize_for_path("..escape"), "--escape");
        assert_eq!(sanitize_for_path("a\\b"), "a-b");
    }

    #[test]
    fn sanitize_path_preserves_safe_names() {
        assert_eq!(sanitize_for_path("my-skill-v2"), "my-skill-v2");
        assert_eq!(sanitize_for_path("debug"), "debug");
    }

    // ── Aliases / effort / agent_type frontmatter tests ─────────────────

    #[test]
    fn parse_aliases_from_frontmatter() {
        let content = r#"---
name: code-review
description: "Review code"
aliases:
  - cr
  - review
---
Do the review.
"#;
        let (manifest, _) = parse_skill_md(content).unwrap();
        assert_eq!(manifest.aliases, vec!["cr", "review"]);
    }

    #[test]
    fn parse_effort_named_from_frontmatter() {
        let content = r#"---
name: deep-think
description: "Think hard"
effort: high
---
Think carefully.
"#;
        let (manifest, _) = parse_skill_md(content).unwrap();
        assert!(matches!(manifest.effort, Some(EffortLevel::High)));
    }

    #[test]
    fn parse_effort_numeric_from_frontmatter() {
        let content = r#"---
name: custom-effort
description: "Custom"
effort: "200"
---
Instructions.
"#;
        let (manifest, _) = parse_skill_md(content).unwrap();
        assert!(matches!(manifest.effort, Some(EffortLevel::Custom(200))));
    }

    #[test]
    fn parse_agent_type_from_frontmatter() {
        let content = r#"---
name: researcher
description: "Research skill"
agent_type: researcher
---
Do research.
"#;
        let (manifest, _) = parse_skill_md(content).unwrap();
        assert_eq!(manifest.agent_type.as_deref(), Some("researcher"));
    }

    #[test]
    fn parse_all_cc_fields_together() {
        let content = r#"---
name: full-cc
description: "Full CC-compatible"
aliases:
  - fc
  - full
effort: max
agent_type: coder
model: "claude-sonnet-4-20250514"
---
Full instructions.
"#;
        let (manifest, _) = parse_skill_md(content).unwrap();
        assert_eq!(manifest.aliases, vec!["fc", "full"]);
        assert!(matches!(manifest.effort, Some(EffortLevel::Max)));
        assert_eq!(manifest.agent_type.as_deref(), Some("coder"));
        assert_eq!(manifest.model.as_deref(), Some("claude-sonnet-4-20250514"));
    }

    #[test]
    fn missing_cc_fields_default_to_none() {
        let content = r#"---
name: minimal
description: "Minimal skill"
---
Just instructions.
"#;
        let (manifest, _) = parse_skill_md(content).unwrap();
        assert!(manifest.aliases.is_empty());
        assert!(manifest.effort.is_none());
        assert!(manifest.agent_type.is_none());
    }

    #[test]
    fn invalid_effort_ignored_in_frontmatter() {
        let content = r#"---
name: bad-effort
description: "Bad effort"
effort: "not-a-level"
---
Instructions.
"#;
        let (manifest, _) = parse_skill_md(content).unwrap();
        assert!(manifest.effort.is_none());
    }
}
