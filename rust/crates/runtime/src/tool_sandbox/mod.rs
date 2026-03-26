//! # Tool Sandbox
//!
//! Security boundary enforcement for agent tool execution.
//!
//! ## Layers
//!
//! 1. **Path validation** — canonicalize and check against project boundary
//! 2. **Command sandboxing** — env filtering, resource limits, restricted bash
//! 3. **Policy engine** — configurable per-session security rules

mod command;
mod path;
mod policy;

pub use command::{
    CommandRisk, SandboxCommandError, analyze_command_risks, filter_environment, sandbox_command,
    wrap_command_with_limits,
};
pub use path::{SandboxPathError, validate_path};
pub use policy::{SandboxMode, SandboxPolicy};
