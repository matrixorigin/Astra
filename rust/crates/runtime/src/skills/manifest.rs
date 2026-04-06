//! Skill manifest — the universal descriptor for a skill regardless of source.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use super::hooks::SkillHooks;
use super::version::{Dependency, Version};

// Re-export verification types from services for use in skill manifests.
pub use astra_services::{VerificationCriterion, VerificationResult, VerifierKind};

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

    #[deprecated(note = "use Display (to_string()) instead")]
    pub fn as_str(&self) -> String {
        self.to_string()
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
    /// Ordered pipeline of skills to execute sequentially.
    ///
    /// When present, invoking this skill runs each step in order,
    /// threading the output of each step into the next as context.
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
///
/// Affects default permission level, marketplace ranking weight,
/// and budget priority during skill selection.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TrustTier {
    /// Platform team — built-in, CI-tested. Full trust.
    Bundled,
    /// Approved publisher — code review + automated scan. High trust.
    Verified,
    /// Any user — automated scan only. Medium trust.
    Community,
    /// Anonymous — no verification. Low trust, prompted on use.
    Unverified,
}

impl Default for TrustTier {
    fn default() -> Self {
        TrustTier::Unverified
    }
}

impl TrustTier {
    /// Numeric weight used in the marketplace ranking algorithm.
    /// Higher = more trusted.
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

    /// Check if this skill is compatible with the current runtime environment.
    ///
    /// Returns a list of compatibility issues (empty = fully compatible).
    /// `available_capabilities` should list the tools/features currently available.
    pub fn check_compatibility(
        &self,
        runtime_version: &str,
        available_capabilities: &[&str],
    ) -> Vec<CompatibilityIssue> {
        let mut issues = Vec::new();
        if let Some(ref compat) = self.compatibility {
            // Check runtime version
            if let Some(ref min_ver) = compat.min_runtime_version {
                if !version_satisfies(runtime_version, min_ver) {
                    issues.push(CompatibilityIssue::RuntimeVersion {
                        required: min_ver.clone(),
                        actual: runtime_version.to_string(),
                    });
                }
            }

            // Check required capabilities
            for cap in &compat.required_capabilities {
                if !available_capabilities.contains(&cap.as_str()) {
                    issues.push(CompatibilityIssue::MissingCapability(cap.clone()));
                }
            }

            // Check platform
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

/// Simple semver check: does `actual` satisfy `>= required`?
/// Only compares major.minor.patch (ignores pre-release).
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
            triggers: vec!["test".into(), "demo".into()],
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
    fn compatibility_check_platform_mismatch() {
        let m = SkillManifest {
            compatibility: Some(CompatibilityInfo {
                platforms: vec!["windows".into()],
                ..Default::default()
            }),
            ..Default::default()
        };
        let issues = m.check_compatibility("1.0.0", &[]);
        if cfg!(target_os = "windows") {
            assert!(issues.is_empty());
        } else {
            assert_eq!(issues.len(), 1);
            assert!(matches!(
                issues[0],
                CompatibilityIssue::UnsupportedPlatform { .. }
            ));
        }
    }

    #[test]
    fn version_satisfies_basic() {
        assert!(version_satisfies("1.0.0", "1.0.0"));
        assert!(version_satisfies("2.0.0", "1.0.0"));
        assert!(version_satisfies("1.1.0", "1.0.0"));
        assert!(!version_satisfies("0.9.0", "1.0.0"));
        assert!(!version_satisfies("1.0.0", "1.0.1"));
    }

    // ── EffortLevel tests ───────────────────────────────────────────────

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
        assert!(EffortLevel::parse("256").is_none()); // u8 overflow
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
