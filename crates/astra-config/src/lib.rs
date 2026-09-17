//! Runtime configuration and user profiles.
//!
//! Standalone crate with zero runtime infrastructure dependencies.

pub mod config_overlay;
pub mod config_version_cli;
pub mod config_versions;
pub mod governed_mutation;
pub mod json_mutation;
pub mod lock_ext;
pub mod runtime_config;
pub mod user_profile;

// Re-exports for ergonomic cross-crate use.
pub use governed_mutation::{
    GovernedConfigInvariantValidation, GovernedConfigMutation, GovernedConfigMutationError,
    GovernedConfigPath, GovernedConfigValueType, apply_governed_config_mutation,
    governed_config_invariant_validation, normalized_config_drift,
    validate_governed_config_candidate,
};
pub use json_mutation::{
    JsonPathMutationError, read_existing_json_path, replace_existing_json_path,
};
pub use runtime_config::{
    EffectiveToolPolicy, ModelPolicyProfile, RuntimeConfig, SafetyConfig, ToolPolicyConfig,
    ToolSurfaceConfig, TrustModeSerde,
};
