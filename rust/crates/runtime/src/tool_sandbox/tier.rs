//! Tool tier classification for tiered execution routing (Phase 5.1).
//!
//! Tools are classified into tiers based on their risk profile:
//!
//! | Tier | Description | Isolation |
//! |------|-------------|-----------|
//! | InProcess | Memory ops, search, read-only | None — runs in tokio task |
//! | Sandboxed | File writes, grep, glob | Workspace boundary + env filter |
//! | Isolated | Bash, git commit, delete | Namespace + cgroup limits |
//!
//! The [`ToolTier`] drives execution routing in the `ServerToolExecutor`:
//! - `InProcess` tools run directly (fastest, no subprocess overhead).
//! - `Sandboxed` tools run as subprocesses with path/env restrictions.
//! - `Isolated` tools run via [`ProcessIsolation`] with Linux namespaces.

use super::SandboxMode;

/// Execution tier for a tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolTier {
    /// Safe for in-process execution (read-only, no side effects).
    InProcess,
    /// Needs subprocess with workspace boundary enforcement.
    Sandboxed,
    /// Needs full subprocess isolation (namespaces, cgroups).
    Isolated,
}

/// Classify a tool by name.  Unknown tools default to `Isolated`.
pub fn classify_tool(name: &str) -> ToolTier {
    match name {
        // ── Tier 0: In-process (no subprocess needed) ────────────────
        "memory_retrieve" | "memory_store" | "memory_search" | "memory_purge"
        | "memory_correct" | "memory_profile" | "memory_feedback" | "web_search" | "web_fetch"
        | "read_file" | "list_dir" | "delegate" => ToolTier::InProcess,

        // ── Tier 1: Sandboxed subprocess ─────────────────────────────
        "grep" | "glob" | "git_status" | "git_diff" | "git_log" | "git_show" | "git_blame" => {
            ToolTier::Sandboxed
        }

        // ── Tier 2: Fully isolated subprocess ────────────────────────
        "bash" | "write_file" | "str_replace" | "delete_file" | "git_commit" => ToolTier::Isolated,

        // Unknown tools get maximum isolation.
        _ => ToolTier::Isolated,
    }
}

/// Adjust the tier based on the session's sandbox mode.
///
/// In `Permissive` mode, everything runs as `InProcess` (backward compat).
/// In `Standard` mode, `Isolated` tools are downgraded to `Sandboxed`
/// (namespace support may not be available in dev environments).
pub fn effective_tier(name: &str, mode: SandboxMode) -> ToolTier {
    let base = classify_tool(name);
    match mode {
        SandboxMode::Permissive => ToolTier::InProcess,
        SandboxMode::Standard => match base {
            ToolTier::Isolated => ToolTier::Sandboxed,
            other => other,
        },
        SandboxMode::Strict => base,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bash_is_isolated() {
        assert_eq!(classify_tool("bash"), ToolTier::Isolated);
    }

    #[test]
    fn read_file_is_in_process() {
        assert_eq!(classify_tool("read_file"), ToolTier::InProcess);
    }

    #[test]
    fn grep_is_sandboxed() {
        assert_eq!(classify_tool("grep"), ToolTier::Sandboxed);
    }

    #[test]
    fn unknown_tool_is_isolated() {
        assert_eq!(classify_tool("unknown_danger"), ToolTier::Isolated);
    }

    #[test]
    fn permissive_mode_downgrades_all() {
        assert_eq!(
            effective_tier("bash", SandboxMode::Permissive),
            ToolTier::InProcess
        );
    }

    #[test]
    fn standard_mode_downgrades_isolated_to_sandboxed() {
        assert_eq!(
            effective_tier("bash", SandboxMode::Standard),
            ToolTier::Sandboxed
        );
        assert_eq!(
            effective_tier("grep", SandboxMode::Standard),
            ToolTier::Sandboxed
        );
    }

    #[test]
    fn strict_mode_preserves_tiers() {
        assert_eq!(
            effective_tier("bash", SandboxMode::Strict),
            ToolTier::Isolated
        );
        assert_eq!(
            effective_tier("grep", SandboxMode::Strict),
            ToolTier::Sandboxed
        );
        assert_eq!(
            effective_tier("read_file", SandboxMode::Strict),
            ToolTier::InProcess
        );
    }
}
