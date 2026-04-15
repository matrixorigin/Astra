//! # Tool Sandbox
//!
//! Security boundary enforcement for agent tool execution.
//!
//! This module re-exports from the `astra-sandbox` crate.
//!
//! ## Layers
//!
//! 1. **Path validation** — canonicalize and check against project boundary
//! 2. **Command sandboxing** — env filtering, resource limits, restricted bash
//! 3. **Policy engine** — configurable per-session security rules

// Re-export everything from astra-sandbox
pub use astra_sandbox::*;
