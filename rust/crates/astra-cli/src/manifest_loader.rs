//! Skill manifest loader: parse YAML manifests → PluginToolEntry registration.
//!
//! Extended manifest format supports `tools:` section alongside existing
//! tables, settings, secrets, and resources declarations.
//!
//! Uses legacy `SkillInstruction`/`SkillMetadata` types pending migration to
//! `astra_runtime::skills::manifest` types.

#![allow(deprecated)]
//!
//! Also supports SKILL.md files for detailed instructions (Claude Code style).
//!
#![allow(dead_code)] // Module provides future extensibility APIs
//! Example manifest with tools:
//! ```text
//! name: kubernetes
//! version: "1.0.0"
//! description: "Kubernetes cluster management"
//! tools:
//!   - name: kubectl_get
//!     description: "Get Kubernetes resources"
//!     triggers: ["kubernetes", "kubectl", "pods", "services", "k8s"]
//!     intents: ["System"]
//!     scope: "local"
//!     command: "kubectl get {{resource}} -o {{format}}"
//!     parameters:
//!       - name: resource
//!         type: string
//!         description: "Resource type (pods, services, deployments, etc.)"
//!       - name: format
//!         type: string
//!         description: "Output format"
//!         default: "wide"
//!     required: ["resource"]
//! ```

use astra_runtime::tool_registry::plugin::{PluginRegistry, PluginToolEntry};
use astra_runtime::tool_registry::{IntentType, Scope};
use serde::Deserialize;
use serde_json::{Value, json};
use std::path::Path;

use crate::mcp_client::McpServerConfig;
use crate::skill_instructions::{SkillInstruction, SkillMetadata, parse_skill_md};

// ─── Manifest Types ─────────────────────────────────────────────────────────

/// Skill manifest with optional tool declarations and SKILL.md support.
#[derive(Debug, Clone, Deserialize)]
pub struct ToolManifest {
    pub name: String,
    pub version: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub tools: Vec<ManifestToolDef>,
    /// Path to SKILL.md file relative to manifest (e.g., "SKILL.md").
    #[serde(default)]
    pub instructions_file: Option<String>,
    /// Inline instructions (alternative to instructions_file).
    #[serde(default)]
    pub instructions: Option<String>,
    /// MCP servers to connect to for external tools.
    #[serde(default)]
    pub mcp_servers: Vec<McpServerConfig>,
    // Existing fields (not parsed here, just tolerated)
    #[serde(default)]
    pub tables: Vec<String>,
    #[serde(default)]
    pub settings: Vec<serde_yaml::Value>,
    #[serde(default)]
    pub secrets: Vec<serde_yaml::Value>,
    #[serde(default)]
    pub resources: Option<serde_yaml::Value>,
    #[serde(default)]
    pub requires: Vec<String>,
    #[serde(default)]
    pub depends_on: Vec<String>,
    #[serde(default)]
    pub table_prefix: Option<String>,
    #[serde(default)]
    pub author: Option<String>,
}

/// Tool definition within a skill manifest.
#[derive(Debug, Clone, Deserialize)]
pub struct ManifestToolDef {
    pub name: String,
    pub description: String,
    #[serde(default)]
    pub triggers: Vec<String>,
    #[serde(default)]
    pub intents: Vec<String>,
    #[serde(default = "default_scope")]
    pub scope: String,
    /// Shell command template (e.g., "kubectl get {{resource}}")
    pub command: Option<String>,
    #[serde(default)]
    pub parameters: Vec<ManifestParam>,
    #[serde(default)]
    pub required: Vec<String>,
}

/// Parameter definition for a manifest tool.
#[derive(Debug, Clone, Deserialize)]
pub struct ManifestParam {
    pub name: String,
    #[serde(rename = "type", default = "default_param_type")]
    pub param_type: String,
    pub description: Option<String>,
    #[serde(default)]
    pub default: Option<String>,
}

fn default_scope() -> String {
    "local".to_string()
}

fn default_param_type() -> String {
    "string".to_string()
}

// ─── Conversion ─────────────────────────────────────────────────────────────

/// Parse intent string → IntentType.
fn parse_intent(s: &str) -> Option<IntentType> {
    match s.to_lowercase().as_str() {
        "codeedit" | "code_edit" => Some(IntentType::CodeEdit),
        "coderead" | "code_read" => Some(IntentType::CodeRead),
        "git" => Some(IntentType::Git),
        "github" => Some(IntentType::GitHub),
        "memory" => Some(IntentType::Memory),
        "introspect" => Some(IntentType::Introspect),
        "database" => Some(IntentType::Database),
        "system" => None, // System isn't an IntentType; use scope instead
        _ => None,
    }
}

/// Parse scope string → Scope.
fn parse_scope(s: &str) -> Scope {
    match s.to_lowercase().as_str() {
        "local" => Scope::Local,
        "localgit" | "local_git" | "git" => Scope::LocalGit,
        "external" => Scope::External,
        "crosssession" | "cross_session" | "session" => Scope::CrossSession,
        _ => Scope::Local,
    }
}

/// Generate an OpenAI-compatible JSON schema from a manifest tool definition.
pub fn manifest_to_schema(tool: &ManifestToolDef) -> Value {
    let mut properties = serde_json::Map::new();
    for param in &tool.parameters {
        let mut prop = serde_json::Map::new();
        prop.insert("type".into(), json!(param.param_type));
        if let Some(desc) = &param.description {
            prop.insert("description".into(), json!(desc));
        }
        if let Some(default) = &param.default {
            prop.insert("default".into(), json!(default));
        }
        properties.insert(param.name.clone(), Value::Object(prop));
    }

    json!({
        "type": "function",
        "function": {
            "name": tool.name,
            "description": tool.description,
            "parameters": {
                "type": "object",
                "properties": properties,
                "required": tool.required
            }
        }
    })
}

/// Convert a manifest tool definition → PluginToolEntry.
pub fn manifest_tool_to_entry(skill_name: &str, tool: &ManifestToolDef) -> PluginToolEntry {
    let schema = manifest_to_schema(tool);
    let schema_str = serde_json::to_string(&schema).unwrap_or_default();
    let schema_tokens = (schema_str.len() as u32) / 4;

    PluginToolEntry {
        name: tool.name.clone(),
        description: tool.description.clone(),
        triggers: tool.triggers.clone(),
        pinned: false,
        intents: tool
            .intents
            .iter()
            .filter_map(|s| parse_intent(s))
            .collect(),
        scope: parse_scope(&tool.scope),
        schema,
        schema_tokens,
        source: format!("skills/{}", skill_name),
        enabled: true,
    }
}

// ─── File Loading ───────────────────────────────────────────────────────────

/// Load a tool manifest from a YAML file path.
pub fn load_manifest(path: &Path) -> Result<ToolManifest, String> {
    let content =
        std::fs::read_to_string(path).map_err(|e| format!("Failed to read manifest: {e}"))?;
    parse_manifest(&content)
}

/// Parse a manifest from a YAML string.
pub fn parse_manifest(yaml: &str) -> Result<ToolManifest, String> {
    serde_yaml::from_str(yaml).map_err(|e| format!("Failed to parse manifest: {e}"))
}

// ─── Loaded Skill (Manifest + Instructions) ─────────────────────────────────

/// A fully loaded skill with manifest and optional instructions.
#[derive(Debug, Clone)]
pub struct LoadedSkill {
    /// Skill name (from manifest).
    pub name: String,
    /// Skill directory path.
    pub path: std::path::PathBuf,
    /// Parsed manifest.
    pub manifest: ToolManifest,
    /// Parsed SKILL.md instructions (if present).
    pub instructions: Option<SkillInstruction>,
    /// Metadata for quick access (Level 1).
    pub metadata: SkillMetadata,
}

impl LoadedSkill {
    /// Get the effective description (from instructions or manifest).
    pub fn description(&self) -> &str {
        self.instructions
            .as_ref()
            .map(|i| i.description.as_str())
            .unwrap_or(&self.manifest.description)
    }

    /// Get triggers for skill selection.
    pub fn triggers(&self) -> Vec<String> {
        self.instructions
            .as_ref()
            .map(|i| i.triggers.clone())
            .unwrap_or_default()
    }

    /// Get allowed tools (from SKILL.md).
    pub fn allowed_tools(&self) -> Vec<String> {
        self.instructions
            .as_ref()
            .map(|i| i.allowed_tools.clone())
            .unwrap_or_default()
    }

    /// Get instruction text (Level 2 content).
    pub fn instruction_text(&self) -> Option<&str> {
        self.instructions.as_ref().map(|i| i.instructions.as_str())
    }

    /// Get MCP server configurations for this skill.
    pub fn mcp_servers(&self) -> &[McpServerConfig] {
        &self.manifest.mcp_servers
    }

    /// Check if this skill has MCP servers configured.
    pub fn has_mcp_servers(&self) -> bool {
        !self.manifest.mcp_servers.is_empty()
    }
}

/// Load a skill from a directory containing manifest.yaml and optional SKILL.md.
pub fn load_skill(skill_dir: &Path) -> Result<LoadedSkill, String> {
    let manifest_path = skill_dir.join("manifest.yaml");
    let manifest = load_manifest(&manifest_path)?;

    // Try to load SKILL.md
    let instructions = load_skill_instructions(&manifest, skill_dir);

    // Build metadata
    let metadata = if let Some(ref inst) = instructions {
        SkillMetadata::from(inst)
    } else {
        SkillMetadata {
            name: manifest.name.clone(),
            description: manifest.description.clone(),
            triggers: Vec::new(),
            user_invocable: true,
            metadata_tokens: (manifest.name.len() + manifest.description.len()) as u32 / 4,
            ..Default::default()
        }
    };

    Ok(LoadedSkill {
        name: manifest.name.clone(),
        path: skill_dir.to_path_buf(),
        manifest,
        instructions,
        metadata,
    })
}

/// Escape a string for safe YAML double-quoted inclusion.
fn escape_yaml_string(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
        .replace('\t', "\\t")
}

/// Load SKILL.md for a manifest (checks instructions_file or default SKILL.md).
fn load_skill_instructions(manifest: &ToolManifest, skill_dir: &Path) -> Option<SkillInstruction> {
    // Check for inline instructions first
    if let Some(ref inline) = manifest.instructions {
        // Wrap inline instructions in frontmatter format
        // Escape special characters to produce valid YAML
        let escaped_name = escape_yaml_string(&manifest.name);
        let escaped_desc = escape_yaml_string(&manifest.description);
        let content = format!(
            "---\nname: \"{}\"\ndescription: \"{}\"\n---\n{}",
            escaped_name, escaped_desc, inline
        );
        return parse_skill_md(&content).ok();
    }

    // Check for instructions_file path
    let skill_md_path = if let Some(ref file) = manifest.instructions_file {
        skill_dir.join(file)
    } else {
        // Default to SKILL.md
        skill_dir.join("SKILL.md")
    };

    if skill_md_path.exists() {
        let content = std::fs::read_to_string(&skill_md_path).ok()?;
        parse_skill_md(&content).ok()
    } else {
        None
    }
}

/// Discover all skill manifests under a skills directory.
/// Returns `(skill_name, manifest)` pairs.
pub fn discover_manifests(skills_dir: &Path) -> Vec<(String, ToolManifest)> {
    let mut manifests = Vec::new();
    let entries = match std::fs::read_dir(skills_dir) {
        Ok(e) => e,
        Err(_) => return manifests,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            let manifest_path = path.join("manifest.yaml");
            if manifest_path.exists()
                && let Ok(manifest) = load_manifest(&manifest_path)
            {
                manifests.push((manifest.name.clone(), manifest));
            }
        }
    }
    manifests
}

/// Discover and load all skills (with SKILL.md) under a skills directory.
/// Returns fully loaded skills with instructions.
pub fn discover_skills(skills_dir: &Path) -> Vec<LoadedSkill> {
    let mut skills = Vec::new();
    let entries = match std::fs::read_dir(skills_dir) {
        Ok(e) => e,
        Err(_) => return skills,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir()
            && let Ok(skill) = load_skill(&path)
        {
            skills.push(skill);
        }
    }
    skills
}

/// Load all manifests and register their tools into a PluginRegistry.
/// Returns the names of successfully registered tools.
pub fn register_manifest_tools(skills_dir: &Path, registry: &mut PluginRegistry) -> Vec<String> {
    let mut registered = Vec::new();
    let manifests = discover_manifests(skills_dir);
    for (skill_name, manifest) in manifests {
        for tool_def in &manifest.tools {
            let entry = manifest_tool_to_entry(&skill_name, tool_def);
            if registry.register(entry).is_ok() {
                registered.push(tool_def.name.clone());
            }
        }
    }
    registered
}

/// Best-effort skill loading from standard locations.
///
/// Uses [`skill_instructions::skill_search_paths()`] for consistent directory
/// resolution across the CLI:
/// 1. `{cwd}/.astra/skills/`
/// 2. `{cwd}/skills/`
/// 3. `~/.astra/skills/`
///
/// Silently skips if no skills directory exists.
pub fn load_skills_directory(registry: &mut PluginRegistry) {
    for path in &crate::skill_instructions::skill_search_paths() {
        if path.is_dir() {
            register_manifest_tools(path, registry);
        }
    }
}

/// Collect all MCP server configs from skill manifests across search paths.
///
/// Scans the same directories as [`load_skills_directory`], discovers skill
/// manifests, and returns their `mcp_servers` entries (deduped by server name).
pub fn collect_mcp_server_configs() -> Vec<crate::mcp_client::McpServerConfig> {
    let mut seen = std::collections::HashSet::new();
    let mut configs = Vec::new();
    for dir in &crate::skill_instructions::skill_search_paths() {
        if !dir.is_dir() {
            continue;
        }
        for (_skill_name, manifest) in discover_manifests(dir) {
            for server in &manifest.mcp_servers {
                if server.enabled && seen.insert(server.name.clone()) {
                    configs.push(server.clone());
                }
            }
        }
    }
    configs
}

// ─── Shell Command Execution for Manifest Tools ────────────────────────────

/// Expand a command template with parameter values.
///
/// Template format: `kubectl get {{resource}} -o {{format}}`
/// Args: `{"resource": "pods", "format": "json"}`
/// Result: `kubectl get pods -o json`
pub fn expand_command_template(template: &str, args: &Value) -> String {
    let mut result = template.to_string();
    if let Value::Object(map) = args {
        for (key, val) in map {
            let placeholder = format!("{{{{{}}}}}", key);
            let replacement = match val {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            result = result.replace(&placeholder, &replacement);
        }
    }
    // Remove any unexpanded placeholders (missing optional params)
    let re = regex::Regex::new(r"\{\{[^}]+\}\}").unwrap();
    re.replace_all(&result, "").trim().to_string()
}

// ─── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp_client::Transport;

    const SAMPLE_MANIFEST: &str = r#"
name: kubernetes
version: "1.0.0"
description: "Kubernetes cluster management"
tools:
  - name: kubectl_get
    description: "Get Kubernetes resources"
    triggers: ["kubernetes", "kubectl", "pods", "services", "k8s"]
    intents: ["CodeRead"]
    scope: "local"
    command: "kubectl get {{resource}} -o {{format}}"
    parameters:
      - name: resource
        type: string
        description: "Resource type (pods, services, deployments)"
      - name: format
        type: string
        description: "Output format"
        default: "wide"
    required: ["resource"]
  - name: kubectl_apply
    description: "Apply Kubernetes manifests"
    triggers: ["kubernetes", "apply", "deploy", "manifest"]
    intents: ["CodeEdit"]
    scope: "local"
    command: "kubectl apply -f {{file}}"
    parameters:
      - name: file
        type: string
        description: "Path to manifest file"
    required: ["file"]
"#;

    const EXISTING_MANIFEST: &str = r#"
name: github
version: "1.0.0"
description: "GitHub integration — PRs, issues, CI status, code search"
author: "astra-engine"
table_prefix: sk_github
tables:
  - sk_github_repos
  - sk_github_pr_cache
settings:
  - name: api_base_url
    type: string
    default: "https://api.github.com"
secrets:
  - name: github_token
    type: secret
    required: true
resources:
  type: repo
  key_pattern: "{owner}/{name}"
requires:
  - http
depends_on: []
"#;

    // ── Manifest parsing ──

    #[test]
    fn parse_manifest_with_tools() {
        let manifest = parse_manifest(SAMPLE_MANIFEST).unwrap();
        assert_eq!(manifest.name, "kubernetes");
        assert_eq!(manifest.version, "1.0.0");
        assert_eq!(manifest.tools.len(), 2);

        let tool = &manifest.tools[0];
        assert_eq!(tool.name, "kubectl_get");
        assert_eq!(tool.triggers.len(), 5);
        assert_eq!(tool.parameters.len(), 2);
        assert_eq!(tool.required, vec!["resource"]);
    }

    #[test]
    fn parse_existing_manifest_without_tools() {
        let manifest = parse_manifest(EXISTING_MANIFEST).unwrap();
        assert_eq!(manifest.name, "github");
        assert!(
            manifest.tools.is_empty(),
            "existing manifest has no tools section"
        );
        assert_eq!(manifest.tables.len(), 2);
    }

    #[test]
    fn parse_minimal_manifest() {
        let yaml = "name: minimal\nversion: '0.1.0'\n";
        let manifest = parse_manifest(yaml).unwrap();
        assert_eq!(manifest.name, "minimal");
        assert!(manifest.tools.is_empty());
    }

    #[test]
    fn parse_invalid_manifest_returns_error() {
        let result = parse_manifest("not: valid: yaml: [[[");
        assert!(result.is_err());
    }

    // ── Schema generation ──

    #[test]
    fn manifest_to_schema_generates_valid_openai_format() {
        let manifest = parse_manifest(SAMPLE_MANIFEST).unwrap();
        let schema = manifest_to_schema(&manifest.tools[0]);

        assert_eq!(schema["type"], "function");
        assert_eq!(schema["function"]["name"], "kubectl_get");
        assert!(schema["function"]["parameters"]["properties"]["resource"].is_object());
        assert_eq!(
            schema["function"]["parameters"]["required"],
            json!(["resource"])
        );
    }

    #[test]
    fn schema_includes_param_defaults() {
        let manifest = parse_manifest(SAMPLE_MANIFEST).unwrap();
        let schema = manifest_to_schema(&manifest.tools[0]);
        assert_eq!(
            schema["function"]["parameters"]["properties"]["format"]["default"],
            "wide"
        );
    }

    // ── Conversion to PluginToolEntry ──

    #[test]
    fn manifest_tool_to_entry_creates_valid_entry() {
        let manifest = parse_manifest(SAMPLE_MANIFEST).unwrap();
        let entry = manifest_tool_to_entry("kubernetes", &manifest.tools[0]);

        assert_eq!(entry.name, "kubectl_get");
        assert_eq!(entry.source, "skills/kubernetes");
        assert!(entry.enabled);
        assert!(!entry.pinned);
        assert!(entry.intents.contains(&IntentType::CodeRead));
        assert_eq!(entry.scope, Scope::Local);
        assert!(entry.schema_tokens > 0);
    }

    #[test]
    fn manifest_tool_to_entry_default_scope() {
        let manifest = parse_manifest(SAMPLE_MANIFEST).unwrap();
        let entry = manifest_tool_to_entry("k8s", &manifest.tools[1]);
        assert_eq!(entry.scope, Scope::Local);
    }

    // ── Registration into PluginRegistry ──

    #[test]
    fn register_manifest_tools_into_registry() {
        let manifest = parse_manifest(SAMPLE_MANIFEST).unwrap();
        let mut registry = PluginRegistry::new();

        for tool_def in &manifest.tools {
            let entry = manifest_tool_to_entry(&manifest.name, tool_def);
            registry.register(entry).unwrap();
        }

        assert_eq!(registry.len(), 2);
        assert!(registry.get("kubectl_get").is_some());
        assert!(registry.get("kubectl_apply").is_some());
    }

    #[test]
    fn registered_manifest_tools_score_via_tfidf() {
        let manifest = parse_manifest(SAMPLE_MANIFEST).unwrap();
        let mut registry = PluginRegistry::new();

        for tool_def in &manifest.tools {
            let entry = manifest_tool_to_entry(&manifest.name, tool_def);
            registry.register(entry).unwrap();
        }

        let query = astra_runtime::text_tokenize::tokenize("show kubernetes pods");
        let scores = registry.score_all(&query);
        assert!(!scores.is_empty());
        assert_eq!(scores[0].1, "kubectl_get");
    }

    // ── Command template expansion ──

    #[test]
    fn expand_simple_template() {
        let result = expand_command_template(
            "kubectl get {{resource}} -o {{format}}",
            &json!({"resource": "pods", "format": "json"}),
        );
        assert_eq!(result, "kubectl get pods -o json");
    }

    #[test]
    fn expand_removes_unexpanded_placeholders() {
        let result = expand_command_template(
            "kubectl get {{resource}} -o {{format}}",
            &json!({"resource": "pods"}),
        );
        assert_eq!(result, "kubectl get pods -o");
    }

    #[test]
    fn expand_empty_args() {
        let result = expand_command_template("echo hello", &json!({}));
        assert_eq!(result, "echo hello");
    }

    // ── Discovery ──

    #[test]
    fn discover_manifests_handles_missing_dir() {
        let manifests = discover_manifests(Path::new("/nonexistent/path"));
        assert!(manifests.is_empty());
    }

    #[test]
    fn load_skills_directory_handles_no_skills() {
        // Should not panic even if no skills/ directory exists
        let mut registry = PluginRegistry::new();
        load_skills_directory(&mut registry);
        // No crash = success; may or may not find skills depending on environment
    }

    #[test]
    fn register_manifest_tools_from_temp_dir() {
        use tempfile::TempDir;

        let dir = TempDir::new().unwrap();
        let skill_dir = dir.path().join("k8s");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(skill_dir.join("manifest.yaml"), SAMPLE_MANIFEST).unwrap();

        let mut registry = PluginRegistry::new();
        let registered = register_manifest_tools(dir.path(), &mut registry);
        assert_eq!(registered.len(), 2);
        assert!(registered.contains(&"kubectl_get".to_string()));
        assert!(registered.contains(&"kubectl_apply".to_string()));

        // Verify schemas are generated
        let schemas = registry.schemas();
        assert_eq!(schemas.len(), 2);
    }

    // ── SKILL.md integration ──

    const SAMPLE_SKILL_MD: &str = r#"---
name: code-review
description: "Perform a comprehensive code review"
user_invocable: true
triggers:
  - review
  - audit
allowed_tools:
  - read_file
  - git_diff
---
# Code Review

Follow these steps:
1. Check the diff
2. Look for issues
3. Provide feedback
"#;

    const MANIFEST_WITH_SKILL_MD: &str = r#"
name: review
version: "1.0.0"
description: "Code review skill"
instructions_file: SKILL.md
tools: []
"#;

    #[test]
    fn load_skill_with_skill_md() {
        use tempfile::TempDir;

        let dir = TempDir::new().unwrap();
        let skill_dir = dir.path().join("review");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(skill_dir.join("manifest.yaml"), MANIFEST_WITH_SKILL_MD).unwrap();
        std::fs::write(skill_dir.join("SKILL.md"), SAMPLE_SKILL_MD).unwrap();

        let skill = load_skill(&skill_dir).unwrap();
        assert_eq!(skill.name, "review");
        assert!(skill.instructions.is_some());

        let inst = skill.instructions.as_ref().unwrap();
        assert_eq!(inst.name, "code-review");
        assert_eq!(inst.triggers, vec!["review", "audit"]);
        assert!(inst.user_invocable);
        assert!(inst.instructions.contains("Follow these steps"));
    }

    #[test]
    fn discover_skills_finds_skill_md() {
        use tempfile::TempDir;

        let dir = TempDir::new().unwrap();
        let skill_dir = dir.path().join("review");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(skill_dir.join("manifest.yaml"), MANIFEST_WITH_SKILL_MD).unwrap();
        std::fs::write(skill_dir.join("SKILL.md"), SAMPLE_SKILL_MD).unwrap();

        let skills = discover_skills(dir.path());
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name, "review");
        assert!(skills[0].instructions.is_some());
    }

    #[test]
    fn load_skill_without_skill_md() {
        use tempfile::TempDir;

        let dir = TempDir::new().unwrap();
        let skill_dir = dir.path().join("k8s");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(skill_dir.join("manifest.yaml"), SAMPLE_MANIFEST).unwrap();

        let skill = load_skill(&skill_dir).unwrap();
        assert_eq!(skill.name, "kubernetes");
        assert!(skill.instructions.is_none());
        // Should still have metadata from manifest
        assert_eq!(skill.metadata.name, "kubernetes");
    }

    #[test]
    fn loaded_skill_helpers_work() {
        use tempfile::TempDir;

        let dir = TempDir::new().unwrap();
        let skill_dir = dir.path().join("review");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(skill_dir.join("manifest.yaml"), MANIFEST_WITH_SKILL_MD).unwrap();
        std::fs::write(skill_dir.join("SKILL.md"), SAMPLE_SKILL_MD).unwrap();

        let skill = load_skill(&skill_dir).unwrap();

        // description() returns SKILL.md description
        assert_eq!(skill.description(), "Perform a comprehensive code review");

        // triggers() returns SKILL.md triggers
        assert_eq!(skill.triggers(), vec!["review", "audit"]);

        // allowed_tools() returns SKILL.md allowed_tools
        assert_eq!(skill.allowed_tools(), vec!["read_file", "git_diff"]);

        // instruction_text() returns markdown body
        let text = skill.instruction_text().unwrap();
        assert!(text.contains("Follow these steps"));
    }

    #[test]
    fn escape_yaml_string_handles_special_chars() {
        assert_eq!(escape_yaml_string("hello"), "hello");
        assert_eq!(escape_yaml_string("say \"hi\""), "say \\\"hi\\\"");
        assert_eq!(escape_yaml_string("line1\nline2"), "line1\\nline2");
        assert_eq!(escape_yaml_string("tab\there"), "tab\\there");
        assert_eq!(escape_yaml_string("back\\slash"), "back\\\\slash");
    }

    #[test]
    fn inline_instructions_with_special_chars() {
        use tempfile::TempDir;

        // Manifest with special characters in description
        let manifest_with_quotes = r#"
name: vulnerable
version: "1.0.0"
description: 'Test with " quote and newline
character'
instructions: "These are inline instructions"
tools: []
"#;

        let dir = TempDir::new().unwrap();
        let skill_dir = dir.path().join("vulnerable");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(skill_dir.join("manifest.yaml"), manifest_with_quotes).unwrap();

        let skill = load_skill(&skill_dir).unwrap();
        assert_eq!(skill.name, "vulnerable");
        // Should successfully parse despite special characters
        assert!(skill.instructions.is_some());
        let inst = skill.instructions.unwrap();
        // Description should be properly escaped and parsed back
        assert!(inst.description.contains("quote"));
    }

    #[test]
    fn manifest_with_mcp_servers() {
        use tempfile::TempDir;

        let manifest_with_mcp = r#"
name: mcp-skill
version: "1.0.0"
description: "Skill with MCP servers"
mcp_servers:
  - name: filesystem
    description: "File access"
    transport:
      type: stdio
      command: ["npx", "@modelcontextprotocol/server-filesystem"]
      args: ["/workspace"]
  - name: github
    description: "GitHub access"
    enabled: false
    transport:
      type: stdio
      command: ["npx", "@modelcontextprotocol/server-github"]
      env:
        GITHUB_TOKEN: "test-token"
tools: []
"#;

        let dir = TempDir::new().unwrap();
        let skill_dir = dir.path().join("mcp-skill");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(skill_dir.join("manifest.yaml"), manifest_with_mcp).unwrap();

        let skill = load_skill(&skill_dir).unwrap();
        assert_eq!(skill.name, "mcp-skill");
        assert!(skill.has_mcp_servers());
        assert_eq!(skill.mcp_servers().len(), 2);

        let fs_server = &skill.mcp_servers()[0];
        assert_eq!(fs_server.name, "filesystem");
        assert!(fs_server.enabled);
        match &fs_server.transport {
            Transport::Stdio { command, args, .. } => {
                assert_eq!(command[0], "npx");
                assert_eq!(args[0], "/workspace");
            }
            _ => panic!("expected Stdio transport"),
        }

        let gh_server = &skill.mcp_servers()[1];
        assert_eq!(gh_server.name, "github");
        assert!(!gh_server.enabled); // Explicitly disabled
    }

    #[test]
    fn manifest_without_mcp_servers() {
        let manifest = parse_manifest(SAMPLE_MANIFEST).unwrap();
        assert!(manifest.mcp_servers.is_empty());
    }

    // ============================================================================
    // Integration Tests - Complete skill loading pipeline
    // ============================================================================

    #[test]
    fn integration_full_skill_with_all_features() {
        use tempfile::TempDir;

        let dir = TempDir::new().unwrap();
        let skill_dir = dir.path().join("complete-skill");
        std::fs::create_dir_all(skill_dir.join("templates")).unwrap();
        std::fs::create_dir_all(skill_dir.join("scripts")).unwrap();

        // Manifest with all features
        let manifest = r#"
name: complete-skill
version: "2.0.0"
description: "A complete skill demonstrating all features"
tools:
  - name: analyze
    description: "Analyze code"
    parameters:
      - name: path
        type: string
        description: "The path to analyze"
    required:
      - path
mcp_servers:
  - name: code-analyzer
    description: "External code analysis"
    transport:
      type: stdio
      command: ["python", "-m", "code_analyzer"]
"#;

        // SKILL.md with detailed instructions
        let skill_md = r#"---
name: complete-skill
description: "A complete skill demonstrating all features"
user_invocable: true
triggers:
  - complete
  - all-features
allowed_tools:
  - read_file
  - write_file
  - bash
---
# Complete Skill Instructions

This skill demonstrates all the features of the skill system.

## Prerequisites
- Ensure the codebase is checked out
- Run any setup scripts needed

## Step 1: Analysis
1. Read the target files
2. Parse the code structure
3. Identify areas for improvement

## Step 2: Implementation
Apply the suggested changes carefully.
"#;

        std::fs::write(skill_dir.join("manifest.yaml"), manifest).unwrap();
        std::fs::write(skill_dir.join("SKILL.md"), skill_md).unwrap();
        std::fs::write(
            skill_dir.join("templates/report.md"),
            "# Report\n{{ content }}",
        )
        .unwrap();
        std::fs::write(
            skill_dir.join("scripts/analyze.sh"),
            "#!/bin/bash\necho analyzing",
        )
        .unwrap();

        let skill = load_skill(&skill_dir).unwrap();

        // Verify manifest loaded correctly
        assert_eq!(skill.name, "complete-skill");
        assert_eq!(skill.manifest.version, "2.0.0");
        assert_eq!(skill.manifest.tools.len(), 1);
        assert!(skill.has_mcp_servers());
        assert_eq!(skill.mcp_servers().len(), 1);

        // Verify SKILL.md instructions loaded
        assert!(skill.instructions.is_some());
        let inst = skill.instructions.as_ref().unwrap();
        assert_eq!(inst.triggers.len(), 2);
        assert!(inst.triggers.contains(&"complete".to_string()));
        assert_eq!(inst.allowed_tools.len(), 3);
        assert!(inst.instructions.contains("Complete Skill Instructions"));
    }

    #[test]
    fn integration_discover_skills_mixed_formats() {
        use tempfile::TempDir;

        let dir = TempDir::new().unwrap();

        // Skill 1: manifest.yaml only
        let skill1_dir = dir.path().join("manifest-only");
        std::fs::create_dir_all(&skill1_dir).unwrap();
        std::fs::write(
            skill1_dir.join("manifest.yaml"),
            r#"
name: manifest-only
version: "1.0.0"
description: "Skill with manifest only"
tools: []
"#,
        )
        .unwrap();

        // Skill 2: SKILL.md only (should still be discovered via discover_and_register_metadata)
        let skill2_dir = dir.path().join("skill-md-only");
        std::fs::create_dir_all(&skill2_dir).unwrap();
        std::fs::write(
            skill2_dir.join("SKILL.md"),
            r#"---
name: skill-md-only
description: "Skill with SKILL.md only"
triggers:
  - simple
---
Simple instructions.
"#,
        )
        .unwrap();

        // Skill 3: Both manifest.yaml and SKILL.md
        let skill3_dir = dir.path().join("both");
        std::fs::create_dir_all(&skill3_dir).unwrap();
        std::fs::write(
            skill3_dir.join("manifest.yaml"),
            r#"
name: both
version: "1.0.0"
description: "Skill with both files"
tools: []
"#,
        )
        .unwrap();
        std::fs::write(
            skill3_dir.join("SKILL.md"),
            r#"---
name: both
description: "Skill with both files"
triggers:
  - combined
---
Combined instructions.
"#,
        )
        .unwrap();

        // discover_skills should find skills with manifest.yaml
        let discovered = discover_skills(dir.path());
        let names: Vec<_> = discovered.iter().map(|s| s.name.as_str()).collect();

        // Should find manifest-only and both (which have manifest.yaml)
        assert!(names.contains(&"manifest-only"));
        assert!(names.contains(&"both"));
        // Note: skill-md-only doesn't have manifest.yaml, so discover_skills won't find it
        // (it would need to be discovered via skill_instructions::discover_and_register_metadata)
    }

    #[test]
    fn integration_skill_with_inline_and_file_instructions() {
        use tempfile::TempDir;

        let dir = TempDir::new().unwrap();
        let skill_dir = dir.path().join("inline-test");
        std::fs::create_dir_all(&skill_dir).unwrap();

        // Manifest with inline instructions - these take precedence
        let manifest = r#"
name: inline-test
version: "1.0.0"
description: "Test inline instructions"
instructions: |
  Inline instructions from manifest.
  These take precedence over SKILL.md.
tools: []
"#;

        // SKILL.md exists but inline takes precedence
        let skill_md = r#"---
name: inline-test
description: "Test inline instructions"
---
SKILL.md instructions (not used when inline exists).
"#;

        std::fs::write(skill_dir.join("manifest.yaml"), manifest).unwrap();
        std::fs::write(skill_dir.join("SKILL.md"), skill_md).unwrap();

        let skill = load_skill(&skill_dir).unwrap();

        // Inline instructions take precedence over SKILL.md
        assert!(skill.instructions.is_some());
        let inst = skill.instructions.as_ref().unwrap();
        assert!(inst.instructions.contains("Inline instructions"));
        assert!(!inst.instructions.contains("not used"));
    }

    #[test]
    fn integration_skill_fallback_to_skill_md() {
        use tempfile::TempDir;

        let dir = TempDir::new().unwrap();
        let skill_dir = dir.path().join("fallback-test");
        std::fs::create_dir_all(&skill_dir).unwrap();

        // Manifest WITHOUT inline instructions
        let manifest = r#"
name: fallback-test
version: "1.0.0"
description: "Test SKILL.md fallback"
tools: []
"#;

        // SKILL.md should be loaded when no inline instructions
        let skill_md = r#"---
name: fallback-test
description: "Test SKILL.md fallback"
---
SKILL.md instructions loaded because no inline.
"#;

        std::fs::write(skill_dir.join("manifest.yaml"), manifest).unwrap();
        std::fs::write(skill_dir.join("SKILL.md"), skill_md).unwrap();

        let skill = load_skill(&skill_dir).unwrap();

        // SKILL.md should be loaded when no inline instructions exist
        assert!(skill.instructions.is_some());
        let inst = skill.instructions.as_ref().unwrap();
        assert!(inst.instructions.contains("SKILL.md instructions"));
    }

    #[test]
    fn integration_skill_fields_preserved() {
        use tempfile::TempDir;

        let dir = TempDir::new().unwrap();
        let skill_dir = dir.path().join("fields-test");
        std::fs::create_dir_all(&skill_dir).unwrap();

        let manifest = r#"
name: fields-test
version: "1.0.0"
description: "Test field preservation"
author: "Test Author"
table_prefix: "test_"
tools:
  - name: test-tool
    description: "A test tool"
"#;

        std::fs::write(skill_dir.join("manifest.yaml"), manifest).unwrap();

        let skill = load_skill(&skill_dir).unwrap();

        assert_eq!(skill.manifest.author, Some("Test Author".to_string()));
        assert_eq!(skill.manifest.table_prefix, Some("test_".to_string()));
        assert_eq!(skill.manifest.tools.len(), 1);
        assert_eq!(skill.manifest.tools[0].name, "test-tool");
    }

    #[test]
    fn integration_enabled_mcp_servers_only() {
        use tempfile::TempDir;

        let dir = TempDir::new().unwrap();
        let skill_dir = dir.path().join("mcp-enabled");
        std::fs::create_dir_all(&skill_dir).unwrap();

        let manifest = r#"
name: mcp-enabled
version: "1.0.0"
description: "Test MCP server filtering"
mcp_servers:
  - name: enabled-server
    enabled: true
    transport:
      type: stdio
      command: ["echo", "enabled"]
  - name: disabled-server
    enabled: false
    transport:
      type: stdio
      command: ["echo", "disabled"]
  - name: default-enabled
    transport:
      type: stdio
      command: ["echo", "default"]
tools: []
"#;

        std::fs::write(skill_dir.join("manifest.yaml"), manifest).unwrap();

        let skill = load_skill(&skill_dir).unwrap();

        assert_eq!(skill.mcp_servers().len(), 3);

        // Check enabled states
        let enabled = skill.mcp_servers().iter().filter(|s| s.enabled).count();
        let disabled = skill.mcp_servers().iter().filter(|s| !s.enabled).count();

        assert_eq!(enabled, 2); // enabled-server and default-enabled (default = true)
        assert_eq!(disabled, 1); // disabled-server
    }
}
