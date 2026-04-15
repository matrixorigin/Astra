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

// Re-export TrustTier conversion from skills manifest to sandbox TrustTier
impl From<&crate::skills::manifest::TrustTier> for astra_sandbox::TrustTier {
    fn from(tier: &crate::skills::manifest::TrustTier) -> Self {
        match tier {
            crate::skills::manifest::TrustTier::Bundled => astra_sandbox::TrustTier::Bundled,
            crate::skills::manifest::TrustTier::Verified => astra_sandbox::TrustTier::Verified,
            crate::skills::manifest::TrustTier::Community => astra_sandbox::TrustTier::Community,
            crate::skills::manifest::TrustTier::Unverified => astra_sandbox::TrustTier::Unverified,
        }
    }
}
