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
//! cloud, and provider-managed deployments all resolve to the same `RunBinding` shape.

mod binding;
mod capability;
mod policy;
mod provider;
mod runtime_environment;

mod tool;
mod workspace;

pub use binding::*;
pub use capability::*;
pub use policy::*;
pub use provider::*;
pub use runtime_environment::*;
pub use tool::*;
pub use workspace::*;
