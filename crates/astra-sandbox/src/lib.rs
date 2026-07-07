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
mod process_isolation;
mod shell_hardening;

// Re-export TrustTier from astra-skills
pub use astra_skills::manifest::TrustTier;

pub use command::{
    CommandRisk, SandboxCommandError, analyze_command_risks, filter_environment,
    is_rm_catastrophic_rm_path, sandbox_command, wrap_command_with_limits,
};
pub use git_safety::{
    GitSafetyViolation, is_bare_git_repo, is_soft_violation, validate_git_command,
};
pub use path::{SandboxPathError, canonicalize_parent_and_append, normalize_path, validate_path};
pub use policy::{
    IsolationLevel, SandboxPolicy, is_never_readable_path, is_sensitive_path,
    is_sensitive_system_dir,
};
pub use process_isolation::{
    CgroupGuard, IsolatedOutput, IsolationConfig, apply_cgroup, execute_isolated,
};
pub use shell_hardening::{
    DANGEROUS_FILE_PATHS, InternalPathKind, SENSITIVE_ENV_VARS, ShellHardeningConfig,
    build_hardened_command, extract_local_workspace_path_mentions, is_dangerous_file_path,
    is_dangerous_read_path, is_internal_safe_path, is_session_tool_result_artifact_reference,
    is_shell_home_path, is_windows_drive_path, scrub_secrets_from_env,
};
