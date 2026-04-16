//! Runtime configuration, user profiles, and A/B testing infrastructure.
//!
//! Standalone crate with zero runtime infrastructure dependencies.

pub mod ab_testing;
pub mod execution_profile;
pub mod lock_ext;
pub mod runtime_config;
pub mod user_profile;
