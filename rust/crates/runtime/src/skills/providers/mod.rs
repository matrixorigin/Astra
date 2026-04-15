//! Skill providers — concrete implementations of the `SkillProvider` trait.
//!
//! Core providers (LocalSkillProvider, BundledSkillProvider, McpSkillProvider) are
//! now provided by the `astra-skills` crate. This module provides runtime-specific
//! providers that depend on astra-services:
//!
//! | Provider | Source | Priority |
//! |----------|--------|----------|
//! | [`DatabaseSkillProvider`](database::DatabaseSkillProvider) | Database (via `SkillService`) | 2 |
//!
//! Dynamic skills (bundled skills generated at build time) are registered here
//! into the BundledSkillProvider from astra-skills.

pub mod dynamic_skills;

pub mod database;

pub use database::DatabaseSkillProvider;
