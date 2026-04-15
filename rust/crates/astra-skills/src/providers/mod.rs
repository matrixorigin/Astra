//! Skill providers — sources of skills from various locations.
//!
//! - **local**: Load skills from filesystem paths (`.astra/skills/`, `~/.astra/skills/`)
//! - **bundled**: Built-in skills compiled into the binary
//! - **mcp**: Skills from MCP servers (via `skill://` resources)

pub mod bundled;
pub mod local;
pub mod mcp;

pub use bundled::BundledSkillProvider;
pub use local::LocalSkillProvider;
pub use mcp::McpSkillProvider;
