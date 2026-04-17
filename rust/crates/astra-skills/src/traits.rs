//! Core traits for the skill framework.
//!
//! All extensibility points are defined as traits so new skill sources, executors,
//! and resolvers can be added without modifying core code.

use super::manifest::{ExecutionContext, LoadedSkill, SkillManifest, SkillSourceKind};
use async_trait::async_trait;

// ── SkillProvider ────────────────────────────────────────────────────────────

/// A source of skills (local filesystem, bundled, database, MCP, plugin).
///
/// Implementations are registered with the [`UnifiedSkillRegistry`](super::registry::UnifiedSkillRegistry)
/// to aggregate skills from multiple sources.
#[async_trait]
pub trait SkillProvider: Send + Sync {
    /// What kind of source this provider represents.
    fn source_kind(&self) -> SkillSourceKind;

    /// Discover all available skills (metadata only).
    async fn discover(&self) -> Result<Vec<SkillManifest>, SkillError>;

    /// Load a skill's full content by name.
    async fn load(&self, name: &str) -> Result<LoadedSkill, SkillError>;

    /// Refresh the provider's cache (re-scan filesystem, re-query DB, etc.).
    async fn refresh(&self) -> Result<(), SkillError>;
}

// ── SkillExecutor ────────────────────────────────────────────────────────────

/// Context passed to a skill executor during invocation.
#[derive(Clone, Debug)]
pub struct SkillExecutionContext {
    /// The task description or user message that triggered the skill.
    pub task: String,
    /// Additional arguments passed to the skill.
    pub arguments: std::collections::HashMap<String, String>,
    /// Current nested agent/sub-run depth of the caller.
    pub recursion_depth: u8,
}

/// Result of a skill execution.
///
/// Note: `verification_results` is typed as `Vec<serde_json::Value>` in this crate.
/// When used with astra_services, convert to/from `VerificationResult`.
#[derive(Clone, Debug)]
pub struct SkillExecutionResult {
    /// The text output produced by the skill.
    pub output: String,
    /// Number of tokens consumed.
    pub tokens_used: u32,
    /// Number of agentic loop turns (for isolated skills).
    pub turns: u32,
    /// Wall-clock execution time in milliseconds.
    pub duration_ms: u64,
    /// Whether the skill completed successfully.
    pub success: bool,
    /// Per-criterion verification results as JSON (empty if no criteria declared).
    pub verification_results: Vec<serde_json::Value>,
    /// Structured error category (if failed).
    pub error_category: Option<super::manifest::SkillErrorKind>,
}

/// Executes a loaded skill in a specific mode (inline or isolated).
#[async_trait]
pub trait SkillExecutor: Send + Sync {
    /// Execute the skill and return its result.
    async fn execute(
        &self,
        skill: &LoadedSkill,
        context: &SkillExecutionContext,
    ) -> Result<SkillExecutionResult, SkillError>;

    /// Whether this executor supports the given execution context.
    fn supports(&self, context: &ExecutionContext) -> bool;
}

// ── SkillResolver (backward-compatible) ──────────────────────────────────────

/// Lightweight description of a skill for tool schema generation.
#[derive(Clone, Debug)]
pub struct SkillToolInfo {
    pub name: String,
    pub description: String,
    pub when_to_use: Option<String>,
    pub source: super::manifest::SkillSourceKind,
    pub aliases: Vec<String>,
    pub category: Option<String>,
    pub tags: Vec<String>,
    pub triggers: Vec<String>,
}

/// A fully resolved skill ready for execution.
#[derive(Clone, Debug)]
pub struct ResolvedSkill {
    pub name: String,
    pub instructions: String,
    pub model: Option<String>,
    pub max_tokens: Option<u32>,
    pub allowed_tools: Vec<String>,
    pub execution_context: ExecutionContext,
    pub hooks: super::hooks::SkillHooks,
    pub skill_dir: Option<String>,
    pub source: super::manifest::SkillSourceKind,
    /// Success criteria as JSON (typed as VerificationCriterion in astra_services).
    pub success_criteria: Vec<serde_json::Value>,
    pub composition: Option<super::manifest::SkillComposition>,
    pub input_schema: Option<serde_json::Value>,
    pub output_schema: Option<serde_json::Value>,
    pub remote_url: Option<String>,
    pub forward_headers: Vec<String>,
    pub required_headers: Vec<String>,
    pub aliases: Vec<String>,
    pub effort: Option<super::manifest::EffortLevel>,
    pub agent_type: Option<String>,
    pub trust_tier: super::manifest::TrustTier,
}

/// Resolves skill names to instructions.
pub trait SkillResolver: Send + Sync {
    /// Resolve a skill by name, loading instructions if needed.
    fn resolve(&self, name: &str) -> Result<ResolvedSkill, SkillError>;

    /// List available skills for schema generation.
    fn available_skills(&self) -> Vec<SkillToolInfo>;
}

// ── SkillInstaller ───────────────────────────────────────────────────────────

/// Report from checking whether an upgrade is safe.
#[derive(Clone, Debug)]
pub struct UpgradeReport {
    /// Skills that would break if the upgrade proceeds.
    pub breaking: Vec<String>,
    /// New dependencies that would be added.
    pub new_dependencies: Vec<String>,
    /// Whether the upgrade is safe.
    pub safe: bool,
}

/// Manages skill installation lifecycle.
#[async_trait]
pub trait SkillInstaller: Send + Sync {
    async fn install(
        &self,
        name: &str,
        version: &super::version::VersionConstraint,
    ) -> Result<(), SkillError>;

    async fn upgrade(
        &self,
        name: &str,
        to_version: &super::version::Version,
    ) -> Result<(), SkillError>;

    async fn rollback(&self, name: &str) -> Result<(), SkillError>;

    async fn uninstall(&self, name: &str) -> Result<(), SkillError>;

    async fn check_upgrade(
        &self,
        name: &str,
        to: &super::version::Version,
    ) -> Result<UpgradeReport, SkillError>;
}

// ── SkillMarketplace ─────────────────────────────────────────────────────────

/// Filters for marketplace search.
#[derive(Clone, Debug, Default)]
pub struct SearchFilters {
    pub category: Option<String>,
    pub tags: Vec<String>,
    pub author: Option<String>,
    pub min_version: Option<super::version::Version>,
}

/// A downloadable skill package.
#[derive(Clone, Debug)]
pub struct SkillPackage {
    pub manifest: SkillManifest,
    pub content: Vec<u8>,
}

/// Information about a skill in the marketplace.
#[derive(Clone, Debug)]
pub struct MarketplaceSkillInfo {
    pub manifest: SkillManifest,
    pub install_count: u64,
    pub rating: Option<f32>,
    pub published_at: Option<String>,
}

/// Contract for a skill marketplace (discovery, publishing, downloading).
#[async_trait]
pub trait SkillMarketplace: Send + Sync {
    async fn search(
        &self,
        query: &str,
        filters: &SearchFilters,
    ) -> Result<Vec<SkillManifest>, SkillError>;

    async fn publish(&self, manifest: &SkillManifest, package: &[u8]) -> Result<(), SkillError>;

    async fn download(
        &self,
        name: &str,
        version: &super::version::VersionConstraint,
    ) -> Result<SkillPackage, SkillError>;

    async fn get_info(&self, name: &str) -> Result<MarketplaceSkillInfo, SkillError>;
}

// ── SkillError ───────────────────────────────────────────────────────────────

/// Errors that can occur in the skill framework.
#[derive(Clone, Debug)]
pub enum SkillError {
    /// Skill not found.
    NotFound(String),
    /// Failed to load skill.
    LoadFailed(String),
    /// Failed to parse skill.
    ParseFailed(String),
    /// Version constraint not satisfied.
    VersionConflict(String),
    /// Dependency resolution failed.
    DependencyError(String),
    /// Execution failed.
    ExecutionFailed(String),
    /// Permission denied.
    PermissionDenied(String),
    /// Token budget exceeded.
    BudgetExceeded(String),
    /// Generic internal error.
    Internal(String),
}

impl std::fmt::Display for SkillError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SkillError::NotFound(msg) => write!(f, "skill not found: {msg}"),
            SkillError::LoadFailed(msg) => write!(f, "skill load failed: {msg}"),
            SkillError::ParseFailed(msg) => write!(f, "skill parse failed: {msg}"),
            SkillError::VersionConflict(msg) => write!(f, "version conflict: {msg}"),
            SkillError::DependencyError(msg) => write!(f, "dependency error: {msg}"),
            SkillError::ExecutionFailed(msg) => write!(f, "execution failed: {msg}"),
            SkillError::PermissionDenied(msg) => write!(f, "permission denied: {msg}"),
            SkillError::BudgetExceeded(msg) => write!(f, "budget exceeded: {msg}"),
            SkillError::Internal(msg) => write!(f, "internal error: {msg}"),
        }
    }
}

impl std::error::Error for SkillError {}

impl From<String> for SkillError {
    fn from(s: String) -> Self {
        SkillError::Internal(s)
    }
}

impl From<std::io::Error> for SkillError {
    fn from(e: std::io::Error) -> Self {
        match e.kind() {
            std::io::ErrorKind::NotFound => SkillError::NotFound(e.to_string()),
            std::io::ErrorKind::PermissionDenied => SkillError::PermissionDenied(e.to_string()),
            _ => SkillError::Internal(e.to_string()),
        }
    }
}
