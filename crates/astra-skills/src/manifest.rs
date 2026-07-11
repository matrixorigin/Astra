//! Skill manifest — the universal descriptor for a skill regardless of source.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::str::FromStr;

use super::hooks::SkillHooks;
use super::version::{Dependency, Version};

/// Where a skill was loaded from.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SkillSourceKind {
    /// Local filesystem (`SKILL.md` in `.astra/skills/`, `.claude/skills/`, or HOME skill dirs).
    #[default]
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

/// Single source of truth for [`SkillSourceKind`] string serialization.
///
/// Adding a variant to the enum without a row here triggers a compile error
/// in [`SkillSourceKind::as_str`] (the exhaustive `match` keeps the table and
/// enum in sync), so the four shapes `as_str` / `Display` / `FromStr` /
/// `SUPPORTED_FILTERS` derive from one place.
const SKILL_SOURCE_KIND_TABLE: &[(SkillSourceKind, &str)] = &[
    (SkillSourceKind::Local, "local"),
    (SkillSourceKind::Bundled, "bundled"),
    (SkillSourceKind::Database, "database"),
    (SkillSourceKind::Mcp, "mcp"),
    (SkillSourceKind::Plugin, "plugin"),
];

impl SkillSourceKind {
    pub const SUPPORTED_FILTERS: &'static [&'static str] =
        &["local", "bundled", "database", "mcp", "plugin"];

    pub fn as_str(&self) -> &'static str {
        // Exhaustive match so adding a variant fails to compile until the
        // table above grows a matching row.
        match self {
            Self::Local => SKILL_SOURCE_KIND_TABLE[0].1,
            Self::Bundled => SKILL_SOURCE_KIND_TABLE[1].1,
            Self::Database => SKILL_SOURCE_KIND_TABLE[2].1,
            Self::Mcp => SKILL_SOURCE_KIND_TABLE[3].1,
            Self::Plugin => SKILL_SOURCE_KIND_TABLE[4].1,
        }
    }
}

impl std::fmt::Display for SkillSourceKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for SkillSourceKind {
    type Err = String;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        let needle = raw.trim().to_ascii_lowercase();
        SKILL_SOURCE_KIND_TABLE
            .iter()
            .find(|(_, label)| *label == needle)
            .map(|(variant, _)| variant.clone())
            .ok_or_else(|| {
                format!(
                    "unsupported skill source '{raw}'; expected one of: {}",
                    Self::SUPPORTED_FILTERS.join(", ")
                )
            })
    }
}

#[cfg(test)]
mod skill_source_kind_table_tests {
    use super::*;

    /// Pin SUPPORTED_FILTERS to the labels in the variants table.
    /// If they ever drift, this test catches the inconsistency before users do.
    #[test]
    fn supported_filters_matches_variants_table() {
        let from_table: Vec<&'static str> = SKILL_SOURCE_KIND_TABLE
            .iter()
            .map(|(_, label)| *label)
            .collect();
        assert_eq!(SkillSourceKind::SUPPORTED_FILTERS, from_table.as_slice());
    }

    /// Round-trip every row: `as_str ∘ from_str = identity` and vice versa.
    #[test]
    fn variants_round_trip_through_table() {
        for (variant, label) in SKILL_SOURCE_KIND_TABLE {
            assert_eq!(variant.as_str(), *label);
            assert_eq!(SkillSourceKind::from_str(label).unwrap(), *variant);
        }
    }

    #[test]
    fn variants_round_trip_through_display_form() {
        for variant in [
            SkillSourceKind::Local,
            SkillSourceKind::Bundled,
            SkillSourceKind::Database,
            SkillSourceKind::Mcp,
            SkillSourceKind::Plugin,
        ] {
            let encoded = variant.to_string();
            let decoded = SkillSourceKind::from_str(&encoded).expect("round-trip should parse");
            assert_eq!(decoded, variant);
        }
    }
}

/// Execution context for a skill invocation.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ExecutionContext {
    /// Skill instructions are injected inline into the current conversation context.
    #[default]
    Inline,
    /// Skill runs in an isolated sub-agent loop with separate context and token budget.
    Fork,
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
    /// Remote execution endpoint. When set, runtime dispatches skill calls over HTTP.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_url: Option<String>,
    /// Header names to forward from the inbound Astra request to remote callbacks.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub forward_headers: Vec<String>,
    /// Header names that must be present before invoking the remote callback.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub required_headers: Vec<String>,
    /// Machine-executable success criteria as raw JSON.
    /// The actual verification types are defined in astra_services.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub success_criteria: Vec<serde_json::Value>,
    /// Abstract capabilities this skill requires (e.g. "shell_execution", "file_read").
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub required_capabilities: Vec<String>,
    /// Composition metadata.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub composition: Option<SkillComposition>,

    // ── Marketplace fields (Phase 3) ────────────────────────────────────────
    /// Trust tier for this skill. Defaults to `Unverified` for non-bundled skills.
    #[serde(default, skip_serializing_if = "is_default_trust_tier")]
    pub trust_tier: TrustTier,
    /// Publisher identity information.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub publisher: Option<PublisherMetadata>,
    /// Compatibility constraints.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compatibility: Option<CompatibilityInfo>,

    // ── CC-compatible fields ────────────────────────────────────────────────
    /// Alternative names for this skill (resolved during lookup).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub aliases: Vec<String>,
    /// Effort level hint — controls reasoning depth when executing this skill.
    /// Named levels: "low", "medium", "high", "max"; or an integer 0-255.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<EffortLevel>,
    /// Agent type for fork execution (e.g. "general-purpose", "bash-only").
    /// Only meaningful when `execution_context` is `Fork`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_type: Option<String>,
}

fn default_true() -> bool {
    true
}

fn is_default_trust_tier(tier: &TrustTier) -> bool {
    *tier == TrustTier::Unverified
}

/// Effort level for skill execution — controls reasoning depth.
///
/// Matches CC's `EffortValue`: named levels map to model-specific
/// reasoning budgets, or a raw integer (0-255) for fine-grained control.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum EffortLevel {
    Low,
    Medium,
    High,
    Max,
    /// Raw numeric effort (0-255).
    Custom(u8),
}

impl EffortLevel {
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_lowercase().as_str() {
            "low" => Some(Self::Low),
            "medium" => Some(Self::Medium),
            "high" => Some(Self::High),
            "max" => Some(Self::Max),
            _ => s.parse::<u8>().ok().map(Self::Custom),
        }
    }
}

impl std::fmt::Display for EffortLevel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Low => f.write_str("low"),
            Self::Medium => f.write_str("medium"),
            Self::High => f.write_str("high"),
            Self::Max => f.write_str("max"),
            Self::Custom(n) => write!(f, "{n}"),
        }
    }
}

impl Serialize for EffortLevel {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for EffortLevel {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        EffortLevel::parse(&s).ok_or_else(|| {
            serde::de::Error::custom(format!(
                "invalid effort level '{s}'. Valid: low, medium, high, max, or 0-255"
            ))
        })
    }
}

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
    /// Maximum nesting depth for this skill's composition chain.
    #[serde(default)]
    pub max_depth: Option<u32>,
    /// Ordered pipeline of skills to execute sequentially.
    #[serde(default)]
    pub steps: Vec<PipelineStep>,
}

/// A single step in a skill pipeline.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PipelineStep {
    /// Skill name to invoke for this step.
    pub skill: String,
    /// Human-readable label (defaults to the skill name if absent).
    #[serde(default)]
    pub label: Option<String>,
    /// Per-step timeout in seconds (overrides the pipeline's max_duration_sec).
    #[serde(default)]
    pub timeout_sec: Option<u32>,
    /// If true (default), the pipeline stops when this step fails.
    #[serde(default = "default_true_pipeline")]
    pub required: bool,
}

fn default_true_pipeline() -> bool {
    true
}

// ── Phase 3: Marketplace signal types ────────────────────────────────────────

/// Trust tier for marketplace skills.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TrustTier {
    /// Platform team — built-in, CI-tested. Full trust.
    Bundled,
    /// Approved publisher — code review + automated scan. High trust.
    Verified,
    /// Any user — automated scan only. Medium trust.
    Community,
    /// Anonymous — no verification. Low trust, prompted on use.
    #[default]
    Unverified,
}

impl TrustTier {
    /// Numeric weight used in the marketplace ranking algorithm.
    pub fn ranking_weight(&self) -> f64 {
        match self {
            TrustTier::Bundled => 1.0,
            TrustTier::Verified => 0.8,
            TrustTier::Community => 0.5,
            TrustTier::Unverified => 0.2,
        }
    }
}

/// Publisher identity metadata.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct PublisherMetadata {
    /// Publisher account identifier (e.g. "org-matrixorigin").
    #[serde(default)]
    pub account_id: Option<String>,
    /// Whether the publisher has been verified.
    #[serde(default)]
    pub verified: bool,
    /// When this version was published.
    #[serde(default)]
    pub published_at: Option<String>,
}

/// Compatibility constraints for a skill.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct CompatibilityInfo {
    /// Minimum runtime version required (semver string, e.g. "0.9.0").
    #[serde(default)]
    pub min_runtime_version: Option<String>,
    /// Abstract capabilities required (e.g. "shell_execution", "file_read").
    #[serde(default)]
    pub required_capabilities: Vec<String>,
    /// Models this skill was tested against.
    #[serde(default)]
    pub tested_models: Vec<String>,
    /// OS platforms this skill supports (e.g. "linux", "macos").
    #[serde(default)]
    pub platforms: Vec<String>,
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
    /// The isolated child stopped at an incomplete/resumable boundary.
    Interrupted,
    /// The isolated child was cancelled.
    Cancelled,
    /// The isolated child ran and failed.
    ExecutionFailed,
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
            remote_url: None,
            forward_headers: Vec::new(),
            required_headers: Vec::new(),
            success_criteria: Vec::new(),
            required_capabilities: Vec::new(),
            composition: None,
            trust_tier: TrustTier::default(),
            publisher: None,
            compatibility: None,
            aliases: Vec::new(),
            effort: None,
            agent_type: None,
        }
    }
}

impl SkillManifest {
    /// Estimated token count for metadata (name + description + when_to_use).
    pub fn metadata_tokens(&self) -> u32 {
        let mut text = format!("{} {}", self.name, self.description);
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

    /// Check if this skill is compatible with the current runtime environment.
    pub fn check_compatibility(
        &self,
        runtime_version: &str,
        available_capabilities: &[&str],
    ) -> Vec<CompatibilityIssue> {
        let mut issues = Vec::new();
        if let Some(ref compat) = self.compatibility {
            if let Some(ref min_ver) = compat.min_runtime_version
                && !version_satisfies(runtime_version, min_ver)
            {
                issues.push(CompatibilityIssue::RuntimeVersion {
                    required: min_ver.clone(),
                    actual: runtime_version.to_string(),
                });
            }
            for cap in &compat.required_capabilities {
                if !available_capabilities.contains(&cap.as_str()) {
                    issues.push(CompatibilityIssue::MissingCapability(cap.clone()));
                }
            }
            if !compat.platforms.is_empty() {
                let current_platform = if cfg!(target_os = "linux") {
                    "linux"
                } else if cfg!(target_os = "macos") {
                    "macos"
                } else if cfg!(target_os = "windows") {
                    "windows"
                } else {
                    "unknown"
                };
                if !compat.platforms.iter().any(|p| p == current_platform) {
                    issues.push(CompatibilityIssue::UnsupportedPlatform {
                        supported: compat.platforms.clone(),
                        current: current_platform.to_string(),
                    });
                }
            }
        }
        issues
    }

    /// Validate the manifest for required fields and well-formedness.
    ///
    /// Returns structured validation errors so callers can react without
    /// string-parsing. An empty list means the manifest is valid.
    pub fn validate(&self) -> Vec<SkillManifestValidationError> {
        let mut errors = Vec::new();

        errors.extend(validate_skill_manifest_core(
            self.name.as_str(),
            self.description.as_str(),
            Some(&self.version),
        ));

        // allowed_tools should have valid tool names if specified
        for tool in &self.allowed_tools {
            if tool.is_empty() {
                errors.push(SkillManifestValidationError::EmptyAllowedTool);
            }
        }

        // input_schema must be valid JSON Schema if provided
        if let Some(ref schema) = self.input_schema
            && !schema.is_object()
        {
            errors.push(SkillManifestValidationError::NonObjectSchema {
                field: "input_schema",
            });
        }

        // output_schema must be valid JSON Schema if provided
        if let Some(ref schema) = self.output_schema
            && !schema.is_object()
        {
            errors.push(SkillManifestValidationError::NonObjectSchema {
                field: "output_schema",
            });
        }

        // remote_url must be valid HTTP/HTTPS if provided
        if let Some(ref url) = self.remote_url
            && !url.starts_with("http://")
            && !url.starts_with("https://")
        {
            errors.push(SkillManifestValidationError::InvalidRemoteUrl(url.clone()));
        }

        // arguments must have non-empty names
        for arg in &self.arguments {
            if arg.name.is_empty() {
                errors.push(SkillManifestValidationError::EmptyArgumentName);
            }
        }

        errors
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SkillManifestValidationError {
    MissingName,
    InvalidName(String),
    MissingDescription,
    MissingVersion,
    InvalidVersion(String),
    EmptyAllowedTool,
    NonObjectSchema { field: &'static str },
    InvalidRemoteUrl(String),
    EmptyArgumentName,
}

impl std::fmt::Display for SkillManifestValidationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingName => write!(f, "name is required"),
            Self::InvalidName(name) => write!(
                f,
                "invalid skill name '{}': must not contain '/', '\\\\', or '..'",
                name
            ),
            Self::MissingDescription => write!(f, "description is required"),
            Self::MissingVersion => write!(f, "version is required"),
            Self::InvalidVersion(version) => {
                write!(f, "version {version} is invalid")
            }
            Self::EmptyAllowedTool => write!(f, "allowed_tools contains an empty tool name"),
            Self::NonObjectSchema { field } => write!(f, "{field} must be a JSON object"),
            Self::InvalidRemoteUrl(url) => {
                write!(f, "remote_url '{url}' must start with http:// or https://")
            }
            Self::EmptyArgumentName => {
                write!(f, "arguments contain an argument with an empty name")
            }
        }
    }
}

pub fn validate_skill_manifest_core(
    name: &str,
    description: &str,
    version: Option<&Version>,
) -> Vec<SkillManifestValidationError> {
    let mut errors = Vec::new();

    if name.is_empty() {
        errors.push(SkillManifestValidationError::MissingName);
    } else if name.contains('/') || name.contains('\\') || name.contains("..") {
        errors.push(SkillManifestValidationError::InvalidName(name.to_string()));
    }

    if description.is_empty() {
        errors.push(SkillManifestValidationError::MissingDescription);
    }

    match version {
        None => errors.push(SkillManifestValidationError::MissingVersion),
        Some(version) if version.major == 0 && version.minor == 0 && version.patch == 0 => {
            errors.push(SkillManifestValidationError::InvalidVersion(format!(
                "{}.{}.{}",
                version.major, version.minor, version.patch
            )));
        }
        Some(_) => {}
    }

    errors
}

/// A compatibility issue detected by `check_compatibility()`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CompatibilityIssue {
    /// Runtime version too old.
    RuntimeVersion { required: String, actual: String },
    /// A required capability is not available.
    MissingCapability(String),
    /// Current platform not in supported list.
    UnsupportedPlatform {
        supported: Vec<String>,
        current: String,
    },
}

impl std::fmt::Display for CompatibilityIssue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CompatibilityIssue::RuntimeVersion { required, actual } => {
                write!(f, "requires runtime >= {required}, have {actual}")
            }
            CompatibilityIssue::MissingCapability(cap) => {
                write!(f, "missing capability: {cap}")
            }
            CompatibilityIssue::UnsupportedPlatform { supported, current } => {
                write!(
                    f,
                    "unsupported platform: {current} (supports: {})",
                    supported.join(", ")
                )
            }
        }
    }
}

fn version_satisfies(actual: &str, required: &str) -> bool {
    let parse = |s: &str| -> (u32, u32, u32) {
        let parts: Vec<u32> = s
            .split('.')
            .take(3)
            .map(|p| p.parse().unwrap_or(0))
            .collect();
        (
            parts.first().copied().unwrap_or(0),
            parts.get(1).copied().unwrap_or(0),
            parts.get(2).copied().unwrap_or(0),
        )
    };
    let a = parse(actual);
    let r = parse(required);
    a >= r
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
            when_to_use: Some("When testing the skill framework".into()),
            ..Default::default()
        };
        let tokens = m.metadata_tokens();
        assert!(tokens > 0);
        assert!(tokens < 100);
    }

    #[test]
    fn trust_tier_ranking_weights() {
        assert_eq!(TrustTier::Bundled.ranking_weight(), 1.0);
        assert_eq!(TrustTier::Verified.ranking_weight(), 0.8);
        assert_eq!(TrustTier::Community.ranking_weight(), 0.5);
        assert_eq!(TrustTier::Unverified.ranking_weight(), 0.2);
    }

    #[test]
    fn default_trust_tier_is_unverified() {
        let m = SkillManifest::default();
        assert_eq!(m.trust_tier, TrustTier::Unverified);
    }

    #[test]
    fn compatibility_check_passes_when_no_constraints() {
        let m = SkillManifest::default();
        let issues = m.check_compatibility("1.0.0", &["shell_execution", "file_read"]);
        assert!(issues.is_empty());
    }

    #[test]
    fn compatibility_check_version_too_old() {
        let m = SkillManifest {
            compatibility: Some(CompatibilityInfo {
                min_runtime_version: Some("2.0.0".into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let issues = m.check_compatibility("1.5.3", &[]);
        assert_eq!(issues.len(), 1);
        assert!(matches!(
            issues[0],
            CompatibilityIssue::RuntimeVersion { .. }
        ));
    }

    #[test]
    fn compatibility_check_missing_capability() {
        let m = SkillManifest {
            compatibility: Some(CompatibilityInfo {
                required_capabilities: vec!["shell_execution".into(), "network_access".into()],
                ..Default::default()
            }),
            ..Default::default()
        };
        let issues = m.check_compatibility("1.0.0", &["shell_execution", "file_read"]);
        assert_eq!(issues.len(), 1);
        assert!(
            matches!(issues[0], CompatibilityIssue::MissingCapability(ref c) if c == "network_access")
        );
    }

    #[test]
    fn effort_parse_named_levels() {
        assert!(matches!(EffortLevel::parse("low"), Some(EffortLevel::Low)));
        assert!(matches!(
            EffortLevel::parse("medium"),
            Some(EffortLevel::Medium)
        ));
        assert!(matches!(
            EffortLevel::parse("high"),
            Some(EffortLevel::High)
        ));
        assert!(matches!(EffortLevel::parse("max"), Some(EffortLevel::Max)));
    }

    #[test]
    fn effort_parse_case_insensitive() {
        assert!(matches!(EffortLevel::parse("LOW"), Some(EffortLevel::Low)));
        assert!(matches!(
            EffortLevel::parse("High"),
            Some(EffortLevel::High)
        ));
        assert!(matches!(EffortLevel::parse("MAX"), Some(EffortLevel::Max)));
    }

    #[test]
    fn effort_parse_numeric() {
        assert!(matches!(
            EffortLevel::parse("0"),
            Some(EffortLevel::Custom(0))
        ));
        assert!(matches!(
            EffortLevel::parse("128"),
            Some(EffortLevel::Custom(128))
        ));
        assert!(matches!(
            EffortLevel::parse("255"),
            Some(EffortLevel::Custom(255))
        ));
    }

    #[test]
    fn effort_parse_invalid_returns_none() {
        assert!(EffortLevel::parse("invalid").is_none());
        assert!(EffortLevel::parse("").is_none());
        assert!(EffortLevel::parse("256").is_none());
        assert!(EffortLevel::parse("-1").is_none());
    }

    #[test]
    fn effort_roundtrip_as_str() {
        for (input, expected) in &[
            ("low", "low"),
            ("medium", "medium"),
            ("high", "high"),
            ("max", "max"),
            ("42", "42"),
        ] {
            let level = EffortLevel::parse(input).unwrap();
            assert_eq!(level.to_string(), *expected);
        }
    }

    #[test]
    fn effort_serde_roundtrip() {
        let level = EffortLevel::High;
        let json = serde_json::to_string(&level).unwrap();
        assert_eq!(json, "\"high\"");
        let parsed: EffortLevel = serde_json::from_str(&json).unwrap();
        assert!(matches!(parsed, EffortLevel::High));
    }

    #[test]
    fn effort_serde_custom_roundtrip() {
        let level = EffortLevel::Custom(200);
        let json = serde_json::to_string(&level).unwrap();
        assert_eq!(json, "\"200\"");
        let parsed: EffortLevel = serde_json::from_str(&json).unwrap();
        assert!(matches!(parsed, EffortLevel::Custom(200)));
    }

    #[test]
    fn effort_serde_invalid_rejects() {
        let result: Result<EffortLevel, _> = serde_json::from_str("\"invalid\"");
        assert!(result.is_err());
    }
}
