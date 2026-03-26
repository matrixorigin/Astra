//! Skill manifest loader: parse YAML manifests → PluginToolEntry registration.
//!
//! Extended manifest format supports `tools:` section alongside existing
//! tables, settings, secrets, and resources declarations.
//!
#![allow(dead_code)] // Module provides future extensibility APIs
//! Example manifest with tools:
//! ```yaml
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

use mo_agent_runtime::tool_registry::plugin::{PluginRegistry, PluginToolEntry};
use mo_agent_runtime::tool_registry::{IntentType, Scope};
use serde::Deserialize;
use serde_json::{Value, json};
use std::path::Path;

// ─── Manifest Types ─────────────────────────────────────────────────────────

/// Skill manifest with optional tool declarations.
#[derive(Debug, Clone, Deserialize)]
pub struct ToolManifest {
    pub name: String,
    pub version: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub tools: Vec<ManifestToolDef>,
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
author: "mo-agent-engine"
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

        let query = mo_agent_runtime::text_tokenize::tokenize("show kubernetes pods");
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
}
