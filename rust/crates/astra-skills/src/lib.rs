//! Skill framework core types and providers for Astra.
//!
//! This crate contains the core skill framework:
//! - **manifest**: Skill manifest parsing and types
//! - **loader**: Skill loading from filesystem
//! - **arguments**: Argument substitution
//! - **activation**: Conditional skill tracking
//! - **composition**: Skill composition and chaining
//! - **providers**: Local, bundled, and MCP skill providers
//! - **traits**: Core traits (SkillProvider, SkillExecutor, SkillResolver)
//!
//! # Architecture
//!
//! Skills are loaded from multiple sources (providers) and aggregated by
//! a registry. Each skill has a manifest defining its behavior, tools,
//! and execution context.
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────────┐
//! │                    UnifiedSkillRegistry                        │
//! ├─────────────────────────────────────────────────────────────────┤
//! │  ┌─────────────┐  ┌─────────────┐  ┌─────────────┐             │
//! │  │   Local     │  │   Bundled   │  │    MCP      │   ...       │
//! │  │  Provider   │  │  Provider   │  │  Provider   │             │
//! │  └─────────────┘  └─────────────┘  └─────────────┘             │
//! └─────────────────────────────────────────────────────────────────┘
//! ```

pub mod activation;
pub mod arguments;
pub mod composition;
pub mod hooks;
pub mod loader;
pub mod manifest;
pub mod pack;
pub mod providers;
pub mod quality;
pub mod traits;
pub mod version;

// Re-export core types at crate root
pub use activation::ConditionalSkillTracker;
pub use arguments::substitute_arguments;
pub use manifest::{
    EffortLevel, ExecutionContext, LoadedSkill, SkillErrorKind, SkillManifest, SkillSourceKind,
};
pub use traits::{SkillError, SkillExecutor, SkillProvider, SkillResolver};
