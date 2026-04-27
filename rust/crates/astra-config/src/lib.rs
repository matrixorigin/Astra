//! Runtime configuration and user profiles.
//!
//! Standalone crate with zero runtime infrastructure dependencies.

pub mod execution_profile;
pub mod lock_ext;
pub mod runtime_config;
pub mod user_profile;

// Re-exports for ergonomic cross-crate use.
pub use runtime_config::{
    EffectiveToolPolicy, ModelPolicyProfile, RuntimeConfig, SafetyConfig, ToolSelectionConfig,
    TrustModeSerde,
};
