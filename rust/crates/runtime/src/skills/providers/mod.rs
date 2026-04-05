//! Skill providers — concrete implementations of the `SkillProvider` trait.
//!
//! | Provider | Source | Priority |
//! |----------|--------|----------|
//! | [`LocalSkillProvider`](local::LocalSkillProvider) | Filesystem SKILL.md | Highest (0) |
//! | [`BundledSkillProvider`](bundled::BundledSkillProvider) | Compiled into binary | 1 |
//! | [`DatabaseSkillProvider`](database::DatabaseSkillProvider) | Database (via `SkillService`) | 2 |
//! | [`McpSkillProvider`](mcp::McpSkillProvider) | MCP server resources | 3 |

mod dynamic_skills;

pub mod bundled;
pub mod database;
pub mod local;
pub mod mcp;

pub use bundled::BundledSkillProvider;
pub use database::DatabaseSkillProvider;
pub use local::LocalSkillProvider;
pub use mcp::McpSkillProvider;
