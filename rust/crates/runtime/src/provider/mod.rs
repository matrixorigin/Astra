//! Provider abstraction layer — capability-driven tool routing.
//!
//! The `provider` module defines the vocabulary for describing what
//! execution backends can do and how they are discovered, matched, and
//! dispatched.

pub mod edge_connection;
pub mod server_builtin;
pub mod traits;
pub mod types;

pub use traits::CapabilityProvider;
pub use types::{ProviderKind, ToolCapability, ToolCategory};
