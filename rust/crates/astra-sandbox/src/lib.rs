//! # Tool Sandbox
//!
//! Security boundary enforcement for agent tool execution.
//!
//! ## Layers
//!
//! 1. **Path validation** — canonicalize and check against project boundary
//! 2. **Command sandboxing** — env filtering, resource limits, restricted bash
//! 3. **Policy engine** — configurable per-session security rules

use serde::{Deserialize, Serialize};

mod bash_ast;
mod command;
mod git_safety;
mod path;
mod policy;
mod process_isolation;
mod shell_hardening;
mod tier;

/// Trust tier for skills - determines sandbox policy.
///
/// This is a local copy to avoid circular dependency with astra-skills.
/// Runtime should map from `astra_skills::manifest::TrustTier` when calling
/// `SandboxPolicy::for_trust_tier`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TrustTier {
    /// Platform team — built-in, CI-tested. Full trust.
    Bundled,
    /// Approved publisher — code review + automated scan. High trust.
    Verified,
    /// Any user — automated scan only. Medium trust.
    Community,
    /// Anonymous — no verification. Low trust.
    #[default]
    Unverified,
}

pub use command::{
    CommandRisk, SandboxCommandError, analyze_command_risks, filter_environment, sandbox_command,
    wrap_command_with_limits,
};
pub use git_safety::{
    GitSafetyViolation, is_bare_git_repo, is_soft_violation, validate_git_command,
};
pub use path::{SandboxPathError, validate_path};
pub use policy::{SandboxMode, SandboxPolicy};
pub use process_isolation::{IsolatedOutput, IsolationConfig, execute_isolated};
pub use shell_hardening::{
    DANGEROUS_FILE_PATHS, SENSITIVE_ENV_VARS, ShellHardeningConfig, build_hardened_command,
    is_dangerous_file_path, scrub_secrets_from_env,
};
pub use tier::{ToolTier, classify_tool, effective_tier};
