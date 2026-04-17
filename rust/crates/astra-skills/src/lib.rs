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

// Runtime skill execution and management
pub mod executor;
pub mod improvement;
pub mod verify;

// Re-export key types
pub use executor::{
    InlineSkillExecutor, IsolatedSkillExecutor, SkillExecutionRouter, SkillSubRunExecutor,
    SubRunResult,
};
pub use improvement::{ImprovementProposal, ImprovementTracker, SkillImprovement, TURN_BATCH_SIZE};
pub use verify::SkillVerifier;

/// Detect inline shell command lines in skill instructions.
pub fn has_inline_shell(instructions: &str) -> bool {
    instructions.lines().any(|line| {
        let t = line.trim_start();
        if t == "!" {
            return true;
        }
        if let Some(rest) = t.strip_prefix('!') {
            rest.starts_with(char::is_whitespace)
        } else {
            false
        }
    })
}
