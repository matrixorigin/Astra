//! Runtime environment model for capability-driven tool execution.
//!
//! The central rule is:
//!
//! ```text
//! visible(tool) = binding grants authority
//!              && runtime can perform the effect
//!              && policy allows the effect
//! ```
//!
//! Topology is intentionally not part of this rule. Local, edge-cloud, pure
//! cloud, and runner deployments all resolve to the same `RunBinding` shape.

mod binding;
mod capability;
mod policy;
mod runner_protocol;
mod runtime_environment;
mod scheduler;
mod tool;
mod workspace;

pub use binding::*;
pub use capability::*;
pub use policy::*;
pub use runner_protocol::*;
pub use runtime_environment::*;
pub use scheduler::*;
pub use tool::*;
pub use workspace::*;
