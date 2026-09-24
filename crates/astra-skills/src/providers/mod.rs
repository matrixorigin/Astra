//! Skill providers — sources of skills from various locations.
//!
//! - **local**: Load skills from filesystem paths (`.astra/skills/`, `~/.astra/skills/`)
//! - **mcp**: Skills from MCP servers (via `skill://` resources)

pub mod local;
pub mod mcp;

pub use local::LocalSkillProvider;
pub use mcp::McpSkillProvider;

pub mod database;

pub use database::DatabaseSkillProvider;
