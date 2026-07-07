//! Runtime environment model for capability-driven tool execution.
//!
//! The central rule is:
//!
//! ```text
//! visible(tool) = provider declares an offer
//!              && provider type matches the tool ownership
//!              && binding/runtime can perform the effect
//!              && policy allows the effect
//! selected(tool) = deterministic provider offer chosen before prompt assembly
//! ```
//!
//! Provider/executor topology decides which offers exist and which offer is
//! selected. Prompt-visible schemas stay stable; route metadata is runtime
//! evidence, not part of the model-facing tool contract.

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
