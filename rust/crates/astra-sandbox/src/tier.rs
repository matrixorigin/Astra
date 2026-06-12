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
        "memory" | "web_search" | "web_fetch" | "read_file" | "list_dir" | "delegate" => {
            ToolTier::InProcess
        }

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
    fn classify_tool_known_tiers() {
        for (tool, expected) in [
            ("bash", ToolTier::Isolated),
            ("read_file", ToolTier::InProcess),
            ("grep", ToolTier::Sandboxed),
            ("unknown_danger", ToolTier::Isolated),
        ] {
            assert_eq!(classify_tool(tool), expected, "classify_tool({tool:?})");
        }
    }

    #[test]
    fn effective_tier_by_mode() {
        for (tool, mode, expected) in [
            ("bash", SandboxMode::Permissive, ToolTier::InProcess),
            ("bash", SandboxMode::Standard, ToolTier::Sandboxed),
            ("grep", SandboxMode::Standard, ToolTier::Sandboxed),
            ("bash", SandboxMode::Strict, ToolTier::Isolated),
            ("grep", SandboxMode::Strict, ToolTier::Sandboxed),
            ("read_file", SandboxMode::Strict, ToolTier::InProcess),
        ] {
            assert_eq!(
                effective_tier(tool, mode),
                expected,
                "effective_tier({tool:?}, {mode:?})"
            );
        }
    }
}
