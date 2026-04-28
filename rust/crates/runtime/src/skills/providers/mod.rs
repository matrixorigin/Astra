//! Skill providers — re-export layer.
//!
//! Core providers and database provider are now in the `astra-skills` crate.

pub use astra_skills::providers::database::DatabaseSkillProvider;
pub use astra_skills::providers::dynamic_skills;
