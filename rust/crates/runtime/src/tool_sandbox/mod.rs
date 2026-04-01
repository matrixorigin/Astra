//! # Tool Sandbox
//!
//! Security boundary enforcement for agent tool execution.
//!
//! ## Layers
//!
//! 1. **Path validation** — canonicalize and check against project boundary
//! 2. **Command sandboxing** — env filtering, resource limits, restricted bash
//! 3. **Policy engine** — configurable per-session security rules

mod bash_ast;
mod command;
mod git_safety;
mod path;
mod policy;
mod shell_hardening;

pub use command::{
    CommandRisk, SandboxCommandError, analyze_command_risks, filter_environment, sandbox_command,
    wrap_command_with_limits,
};
pub use git_safety::{GitSafetyViolation, is_bare_git_repo, validate_git_command};
pub use path::{SandboxPathError, validate_path};
pub use policy::{SandboxMode, SandboxPolicy};
pub use shell_hardening::{
    DANGEROUS_FILE_PATHS, SENSITIVE_ENV_VARS, ShellHardeningConfig, build_hardened_command,
    is_dangerous_file_path, scrub_secrets_from_env,
};
