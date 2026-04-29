//! Single source of truth for tool behavioral metadata.
//!
//! Every hardcoded tool-name list in the codebase (stall.rs, turn_guard.rs,
//! parallel_tool_exec.rs, microcompact.rs, headless_tool_assembly.rs,
//! safety_middleware.rs, cloud_approval_policy.rs, concurrency_safety.rs)
//! should derive its answers from queries against this registry.

use std::collections::HashMap;
use std::sync::OnceLock;

pub use astra_turn_types::ToolIdempotency;

/// Display category for CLI status lines — maps tools to formatting groups.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ToolDisplayCategory {
    Github,
    File,
    Shell,
    Search,
    Git,
    Code,
    Mo,
    Memory,
    Utility,
    Other,
}

/// Behavioral category for a tool, ordered by increasing mutation risk.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ToolCategory {
    /// Pure reads — never restrict, safe to parallelize, compactable by default.
    ReadOnly,
    /// Consultative tools — no filesystem mutation but drive agent behavior.
    Consultative,
    /// Mutating tools — modify files, require approval in cloud mode.
    Mutating,
    /// Shell execution — arbitrary side effects, highest risk.
    Shell,
}

impl ToolCategory {
    pub fn is_read_only(self) -> bool {
        matches!(self, Self::ReadOnly)
    }

    pub fn is_parallelizable(self) -> bool {
        matches!(self, Self::ReadOnly | Self::Consultative)
    }

    pub fn is_never_restrict(self) -> bool {
        matches!(self, Self::ReadOnly)
    }

    pub fn is_mutating(self) -> bool {
        matches!(self, Self::Mutating | Self::Shell)
    }

    pub fn is_shell(self) -> bool {
        matches!(self, Self::Shell)
    }
}

/// Fine-grained behavioral flags orthogonal to category.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ToolFlags(u16);

impl ToolFlags {
    pub const COMPACTABLE: Self = Self(1 << 0);
    pub const APPROVAL_REQUIRED: Self = Self(1 << 1);
    pub const EXECUTE_COMMAND: Self = Self(1 << 2);
    pub const CODE_INTEL: Self = Self(1 << 3);
    pub const GIT_READ: Self = Self(1 << 4);
    pub const GITHUB_READ: Self = Self(1 << 5);
    pub const WEB: Self = Self(1 << 6);
    pub const MEMORY: Self = Self(1 << 7);
    pub const EXPLORATION: Self = Self(1 << 8);
    pub const ALIAS: Self = Self(1 << 9);
    pub const MATRIXONE: Self = Self(1 << 10);
    pub const ORCHESTRATION: Self = Self(1 << 11);
    pub const FILE_OP: Self = Self(1 << 12);

    pub const fn empty() -> Self {
        Self(0)
    }

    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    pub const fn contains(self, other: Self) -> bool {
        (self.0 & other.0) == other.0
    }
}

/// Metadata for a single tool — category + flags + idempotency.
#[derive(Debug, Clone, Copy)]
pub struct ToolMeta {
    pub name: &'static str,
    pub category: ToolCategory,
    pub flags: ToolFlags,
    pub idempotency: ToolIdempotency,
}

const fn tool(name: &'static str, category: ToolCategory, flags: ToolFlags) -> ToolMeta {
    ToolMeta {
        name,
        category,
        flags,
        idempotency: match category {
            ToolCategory::ReadOnly | ToolCategory::Consultative => ToolIdempotency::PureRead,
            _ => ToolIdempotency::NonIdempotent,
        },
    }
}

const fn tool_idem(
    name: &'static str,
    category: ToolCategory,
    flags: ToolFlags,
    idempotency: ToolIdempotency,
) -> ToolMeta {
    ToolMeta {
        name,
        category,
        flags,
        idempotency,
    }
}

pub use astra_turn_types::classify_tool_idempotency;

// Shorthand constants for readability in the table.
const RO: ToolCategory = ToolCategory::ReadOnly;
const CO: ToolCategory = ToolCategory::Consultative;
const MU: ToolCategory = ToolCategory::Mutating;
const SH: ToolCategory = ToolCategory::Shell;

const C: ToolFlags = ToolFlags::COMPACTABLE;
const A: ToolFlags = ToolFlags::APPROVAL_REQUIRED;
const AE: ToolFlags = ToolFlags::APPROVAL_REQUIRED.union(ToolFlags::EXECUTE_COMMAND);
const CI: ToolFlags = ToolFlags::CODE_INTEL.union(ToolFlags::COMPACTABLE);
const GR: ToolFlags = ToolFlags::GIT_READ.union(ToolFlags::COMPACTABLE);
const GH: ToolFlags = ToolFlags::GITHUB_READ.union(ToolFlags::COMPACTABLE);
const WB: ToolFlags = ToolFlags::WEB.union(ToolFlags::COMPACTABLE);
const ME: ToolFlags = ToolFlags::MEMORY;
const EX: ToolFlags = ToolFlags::EXPLORATION;
const AL: ToolFlags = ToolFlags::ALIAS;
const MO: ToolFlags = ToolFlags::MATRIXONE;
const OR: ToolFlags = ToolFlags::ORCHESTRATION;
const FI: ToolFlags = ToolFlags::FILE_OP;
const NONE: ToolFlags = ToolFlags::empty();

/// The canonical tool metadata table — single source of truth.
///
/// If a tool isn't listed here, it's treated as `Mutating` with no flags
/// (safe default: not parallelizable, not compactable, not restricted).
static TOOL_TABLE: &[ToolMeta] = &[
    // ── Core filesystem read-only ────────────────────────────────────
    tool("read_file", RO, C.union(EX).union(FI)),
    tool("file_read", RO, C.union(AL).union(FI)),
    tool("ReadFileTool", RO, C.union(AL).union(FI)),
    tool("list_dir", RO, C.union(EX)),
    tool("ListDirTool", RO, C.union(AL)),
    tool("grep", RO, C.union(EX)),
    tool("GrepTool", RO, C.union(AL)),
    tool("glob", RO, C.union(EX)),
    tool("GlobTool", RO, C.union(AL)),
    tool("get_file_contents", RO, C.union(AL).union(FI)),
    tool("search_code", RO, C.union(AL)),
    tool("list_files", RO, C.union(AL)),
    tool("find_files", RO, C.union(AL)),
    tool("view_file", RO, C.union(AL).union(FI)),
    tool("search", RO, C.union(EX)),
    tool("find", RO, C.union(EX)),
    tool("tool_search", RO, C),
    // ── Git read-only ────────────────────────────────────────────────
    tool("git_status", RO, GR),
    tool("git_diff", RO, GR),
    tool("git_log", RO, GR),
    tool("git_show", RO, GR),
    tool("git_blame", RO, GR),
    tool("git_file_history", RO, GR),
    tool("git_contributors", RO, GR),
    tool("git_log_search", RO, GR),
    // ── Code intelligence (LSP-derived, read-only) ───────────────────
    tool("symbols", RO, CI.union(EX)),
    tool("find_definition", RO, CI.union(EX)),
    tool("find_references", RO, CI.union(EX)),
    tool("symbol_search", RO, CI.union(EX)),
    tool("hover_info", RO, CI.union(EX)),
    tool("call_graph", RO, CI.union(EX)),
    tool("type_hierarchy", RO, CI),
    tool("dead_code", RO, CI),
    tool("extract_members", RO, CI),
    tool("lsp", RO, CI.union(EX)),
    // ── GitHub API read-only ─────────────────────────────────────────
    tool("github_list_prs", RO, GH),
    tool("github_get_pr", RO, GH),
    tool("github_ci_status", RO, GH),
    tool("github_list_issues", RO, GH),
    tool("github_get_issue", RO, GH),
    tool("github_repo_stats", RO, GH),
    // ── Web (read-only, compactable) ─────────────────────────────────
    tool("web_fetch", RO, WB),
    tool("WebFetchTool", RO, WB.union(AL)),
    tool("web_search", RO, WB),
    tool("WebSearchTool", RO, WB.union(AL)),
    // ── Memory / retrieval (read-only but not compactable) ───────────
    tool("memory_search", RO, ME),
    tool("memory_retrieve", RO, ME),
    tool("memory_profile", RO, ME),
    // ── MatrixOne read-only ──────────────────────────────────────────
    tool("mo_query", RO, MO),
    // ── Agent info / reflection (read-only) ──────────────────────────
    tool("get_agent_info", RO, C),
    tool("reflect", RO, C),
    tool("context_analysis", RO, OR),
    tool("diagnose", RO, OR),
    // ── Consultative ─────────────────────────────────────────────────
    tool("skill", CO, NONE),
    tool("discover_skills", CO, NONE),
    // ask_user blocks until user responds — retrying would double-prompt.
    // sleep has wall-clock side effects — not safe to retry transparently.
    tool_idem("ask_user", CO, OR, ToolIdempotency::NonIdempotent),
    tool_idem("sleep", CO, OR, ToolIdempotency::NonIdempotent),
    tool("brief", CO, OR),
    tool("query_context", CO, OR),
    // ── Mutating — file writes ───────────────────────────────────────
    tool_idem(
        "write_file",
        MU,
        A.union(FI),
        ToolIdempotency::IdempotentWrite,
    ),
    tool_idem(
        "WriteFileTool",
        MU,
        A.union(AL).union(FI),
        ToolIdempotency::IdempotentWrite,
    ),
    tool("str_replace", MU, A.union(FI)),
    tool("multi_edit", MU, A.union(FI)),
    tool("edit_file", MU, A.union(FI)),
    tool("EditFileTool", MU, A.union(AL).union(FI)),
    tool("apply_patch", MU, A.union(FI)),
    tool("ApplyPatchTool", MU, A.union(AL).union(FI)),
    tool("create_file", MU, A.union(FI)),
    tool("delete_file", MU, A.union(FI)),
    tool("notebook_edit", MU, OR),
    // ── Mutating — git writes ────────────────────────────────────────
    tool("git_commit", MU, A),
    tool("git_revert_commit", MU, A),
    tool("git_stash", MU, A),
    tool("git_checkout_file", MU, NONE),
    tool("git_worktree", MU, NONE),
    // ── Mutating — rollback ──────────────────────────────────────────
    tool("rollback_file_edits", MU, A.union(OR)),
    tool("rollback_database_snapshots", MU, A.union(OR)),
    tool("rollback_turn_actions", MU, A.union(OR)),
    tool("rollback_session_state", MU, OR),
    // ── Mutating — GitHub writes ─────────────────────────────────────
    tool("github_create_issue", MU, A),
    // ── Mutating — memory writes ─────────────────────────────────────
    tool("memory_store", MU, ME),
    tool("memory_correct", MU, ME),
    tool("memory_purge", MU, ME),
    // ── Mutating — MatrixOne writes ──────────────────────────────────
    tool("mo_snapshot", MU, MO),
    tool("mo_branch", MU, MO),
    // ── Mutating — code intelligence writes ──────────────────────────
    tool("rename_symbol", MU, ToolFlags::CODE_INTEL),
    // ── Mutating — orchestration ─────────────────────────────────────
    tool("send_message", MU, OR),
    tool("spawn_agent", MU, OR),
    tool("share_context", MU, OR),
    tool("run_chain", MU, OR),
    tool("run_build_test", MU, OR),
    tool("config", MU, OR),
    tool("adjust_config", MU, OR),
    tool("set_goal", MU, OR),
    tool("prioritize_tool", MU, OR),
    tool("deprioritize_tool", MU, OR),
    tool("compress_context", MU, OR),
    tool("env", MU, OR),
    // ── Mutating — task management ───────────────────────────────────
    tool("task_create", MU, OR),
    tool("task_update", MU, OR),
    tool("task_stop", MU, OR),
    tool("task_list", RO, OR),
    tool("task_get", RO, OR),
    // ── Shell execution (highest risk) ───────────────────────────────
    tool("bash", SH, AE.union(EX)),
    tool("BashTool", SH, AE.union(AL)),
    tool("exec", SH, AE.union(EX)),
    tool("run_command", SH, AE.union(EX)),
    tool("shell", SH, AE.union(EX)),
    tool("powershell", SH, AE.union(EX)),
    tool("PowerShellTool", SH, AE.union(AL).union(EX)),
];

/// Runtime-queryable registry backed by the static table.
pub struct ToolRegistry {
    by_name: HashMap<&'static str, &'static ToolMeta>,
}

impl ToolRegistry {
    fn new() -> Self {
        let mut by_name = HashMap::with_capacity(TOOL_TABLE.len());
        for meta in TOOL_TABLE {
            by_name.insert(meta.name, meta);
        }
        Self { by_name }
    }

    pub fn get(&self, name: &str) -> Option<&'static ToolMeta> {
        self.by_name.get(name).copied()
    }

    pub fn category(&self, name: &str) -> ToolCategory {
        self.get(name)
            .map(|m| m.category)
            .unwrap_or(ToolCategory::Mutating)
    }

    pub fn flags(&self, name: &str) -> ToolFlags {
        self.get(name)
            .map(|m| m.flags)
            .unwrap_or(ToolFlags::empty())
    }

    pub fn idempotency(&self, name: &str) -> ToolIdempotency {
        self.get(name)
            .map(|m| m.idempotency)
            .unwrap_or(ToolIdempotency::NonIdempotent)
    }

    // ── Derived queries (replace all hardcoded lists) ────────────────

    pub fn is_read_only(&self, name: &str) -> bool {
        self.category(name).is_read_only()
    }

    pub fn is_never_restrict(&self, name: &str) -> bool {
        self.category(name).is_never_restrict()
    }

    pub fn is_parallelizable(&self, name: &str) -> bool {
        self.category(name).is_parallelizable()
    }

    pub fn is_compactable(&self, name: &str) -> bool {
        self.flags(name).contains(ToolFlags::COMPACTABLE)
    }

    pub fn is_approval_required(&self, name: &str) -> bool {
        self.flags(name).contains(ToolFlags::APPROVAL_REQUIRED)
    }

    pub fn is_execute_command(&self, name: &str) -> bool {
        self.flags(name).contains(ToolFlags::EXECUTE_COMMAND)
    }

    pub fn is_shell(&self, name: &str) -> bool {
        self.category(name).is_shell()
    }

    pub fn is_exploration(&self, name: &str) -> bool {
        self.flags(name).contains(ToolFlags::EXPLORATION)
    }

    pub fn is_consultative(&self, name: &str) -> bool {
        self.category(name) == ToolCategory::Consultative
    }

    pub fn is_exploration_or_consultative(&self, name: &str) -> bool {
        self.is_exploration(name) || self.is_consultative(name)
    }

    pub fn is_code_intel(&self, name: &str) -> bool {
        self.flags(name).contains(ToolFlags::CODE_INTEL)
    }

    pub fn is_git_read(&self, name: &str) -> bool {
        self.flags(name).contains(ToolFlags::GIT_READ)
    }

    pub fn is_github_read(&self, name: &str) -> bool {
        self.flags(name).contains(ToolFlags::GITHUB_READ)
    }

    pub fn is_alias(&self, name: &str) -> bool {
        self.flags(name).contains(ToolFlags::ALIAS)
    }

    pub fn is_matrixone(&self, name: &str) -> bool {
        self.flags(name).contains(ToolFlags::MATRIXONE)
    }

    pub fn is_orchestration(&self, name: &str) -> bool {
        self.flags(name).contains(ToolFlags::ORCHESTRATION)
    }

    pub fn is_file_op(&self, name: &str) -> bool {
        self.flags(name).contains(ToolFlags::FILE_OP)
    }

    /// Display category for CLI status lines and headless output.
    ///
    /// Derived from flags — no separate hardcoded match needed.
    pub fn display_category(&self, name: &str) -> ToolDisplayCategory {
        if name.starts_with("github_") {
            return ToolDisplayCategory::Github;
        }
        if name.starts_with("memoria_") || name.starts_with("memory_") {
            return ToolDisplayCategory::Memory;
        }
        let flags = self.flags(name);
        let category = self.category(name);
        if flags.contains(ToolFlags::CODE_INTEL) {
            ToolDisplayCategory::Code
        } else if flags.contains(ToolFlags::GIT_READ) || name.starts_with("git_") {
            ToolDisplayCategory::Git
        } else if flags.contains(ToolFlags::MATRIXONE) {
            ToolDisplayCategory::Mo
        } else if flags.contains(ToolFlags::MEMORY) {
            ToolDisplayCategory::Memory
        } else if category.is_shell() || name == "run_build_test" {
            ToolDisplayCategory::Shell
        } else if flags.contains(ToolFlags::FILE_OP) {
            ToolDisplayCategory::File
        } else if flags.contains(ToolFlags::WEB)
            || matches!(
                name,
                "search"
                    | "grep"
                    | "GrepTool"
                    | "find"
                    | "glob"
                    | "GlobTool"
                    | "list_dir"
                    | "ListDirTool"
                    | "tool_search"
                    | "search_code"
                    | "list_files"
                    | "find_files"
            )
        {
            ToolDisplayCategory::Search
        } else if flags.contains(ToolFlags::ORCHESTRATION)
            || matches!(
                name,
                "get_agent_info" | "reflect" | "skill" | "discover_skills"
            )
        {
            ToolDisplayCategory::Utility
        } else {
            ToolDisplayCategory::Other
        }
    }

    // ── Iterators ───────────────────────────────────────────────────

    pub fn read_only_names(&self) -> Vec<&'static str> {
        TOOL_TABLE
            .iter()
            .filter(|m| m.category.is_read_only())
            .map(|m| m.name)
            .collect()
    }

    pub fn compactable_names(&self) -> Vec<&'static str> {
        TOOL_TABLE
            .iter()
            .filter(|m| m.flags.contains(ToolFlags::COMPACTABLE))
            .map(|m| m.name)
            .collect()
    }

    pub fn approval_required_names(&self) -> Vec<&'static str> {
        TOOL_TABLE
            .iter()
            .filter(|m| {
                m.flags.contains(ToolFlags::APPROVAL_REQUIRED)
                    && !m.flags.contains(ToolFlags::ALIAS)
            })
            .map(|m| m.name)
            .collect()
    }

    pub fn execute_command_names(&self) -> Vec<&'static str> {
        TOOL_TABLE
            .iter()
            .filter(|m| {
                m.flags.contains(ToolFlags::EXECUTE_COMMAND) && !m.flags.contains(ToolFlags::ALIAS)
            })
            .map(|m| m.name)
            .collect()
    }

    pub fn shell_names(&self) -> Vec<&'static str> {
        TOOL_TABLE
            .iter()
            .filter(|m| m.category.is_shell() && !m.flags.contains(ToolFlags::ALIAS))
            .map(|m| m.name)
            .collect()
    }

    pub fn exploration_names(&self) -> Vec<&'static str> {
        TOOL_TABLE
            .iter()
            .filter(|m| m.flags.contains(ToolFlags::EXPLORATION))
            .map(|m| m.name)
            .collect()
    }

    pub fn canonical_names(&self) -> Vec<&'static str> {
        TOOL_TABLE
            .iter()
            .filter(|m| !m.flags.contains(ToolFlags::ALIAS))
            .map(|m| m.name)
            .collect()
    }

    pub fn headless_read_only_names(&self) -> Vec<&'static str> {
        TOOL_TABLE
            .iter()
            .filter(|m| {
                m.category.is_read_only()
                    && !m.flags.contains(ToolFlags::WEB)
                    && !m.flags.contains(ToolFlags::MEMORY)
                    && !m.flags.contains(ToolFlags::ALIAS)
                    && !m.flags.contains(ToolFlags::MATRIXONE)
                    && !m.flags.contains(ToolFlags::ORCHESTRATION)
            })
            .map(|m| m.name)
            .collect()
    }
}

/// Process-wide singleton registry.
pub fn registry() -> &'static ToolRegistry {
    static INSTANCE: OnceLock<ToolRegistry> = OnceLock::new();
    INSTANCE.get_or_init(ToolRegistry::new)
}

// ── Args-aware classification ─────────────────────────────────────────
//
// Claude Code's killer feature: `bash "git status"` is safe to run in
// parallel and needs no approval, while `bash "rm -rf"` is serial and
// gated. This struct captures all classification dimensions in one call
// so callers never disagree.

/// Complete classification of a tool invocation (name + args).
///
/// Produced by [`classify`] — the single entry point that replaces all
/// individual `is_*` queries when args are available. Each field is a
/// resolved decision, not a lookup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ToolClassification {
    pub category: ToolCategory,
    pub flags: ToolFlags,
    /// Safe to run concurrently with other parallelizable tools.
    pub parallelizable: bool,
    /// Requires user approval before execution (cloud mode).
    pub approval_required: bool,
    /// Result content can be compacted after the LLM acts on it.
    pub compactable: bool,
    /// Must never be removed from the model's tool set.
    pub never_restrict: bool,
    /// Counts as exploration for stall detection.
    pub exploration: bool,
    /// Retry safety — determines if a failed call can be retried.
    pub idempotency: ToolIdempotency,
}

/// Classify a tool invocation by name and arguments.
///
/// This is the primary entry point for all classification decisions.
/// For shell tools, inspects `args["command"]` to determine if the
/// command is read-only (e.g. `git status`, `ls`, `cargo check`),
/// which unlocks parallel execution and approval bypass.
pub fn classify(name: &str, args: Option<&serde_json::Value>) -> ToolClassification {
    let r = registry();
    let meta_category = r.category(name);
    let meta_flags = r.flags(name);

    let shell_read_only = meta_category.is_shell()
        && args
            .and_then(|a| a.get("command"))
            .and_then(|v| v.as_str())
            .is_some_and(crate::cloud_approval_policy::bash_command_is_read_only);

    let parallelizable = if shell_read_only {
        true
    } else {
        meta_category.is_parallelizable()
    };

    let approval_required = if shell_read_only {
        false
    } else {
        meta_flags.contains(ToolFlags::APPROVAL_REQUIRED)
    };

    let compactable = if shell_read_only {
        true
    } else {
        meta_flags.contains(ToolFlags::COMPACTABLE)
    };

    let never_restrict = meta_category.is_never_restrict();

    let exploration = if shell_read_only {
        true
    } else {
        meta_flags.contains(ToolFlags::EXPLORATION) || meta_category == ToolCategory::Consultative
    };

    let idempotency = if shell_read_only {
        ToolIdempotency::PureRead
    } else {
        r.idempotency(name)
    };

    ToolClassification {
        category: if shell_read_only {
            ToolCategory::ReadOnly
        } else {
            meta_category
        },
        flags: meta_flags,
        parallelizable,
        approval_required,
        compactable,
        never_restrict,
        exploration,
        idempotency,
    }
}

/// Convenience: classify by name only (no args). Equivalent to
/// `classify(name, None)` — used when args are not available.
pub fn classify_name(name: &str) -> ToolClassification {
    classify(name, None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_file_is_read_only_never_restrict_compactable_parallelizable() {
        let r = registry();
        assert!(r.is_read_only("read_file"));
        assert!(r.is_never_restrict("read_file"));
        assert!(r.is_compactable("read_file"));
        assert!(r.is_parallelizable("read_file"));
        assert!(!r.is_approval_required("read_file"));
        assert!(!r.is_shell("read_file"));
    }

    #[test]
    fn bash_is_shell_approval_required_execute_command_exploration() {
        let r = registry();
        assert!(r.is_shell("bash"));
        assert!(r.is_approval_required("bash"));
        assert!(r.is_execute_command("bash"));
        assert!(r.is_exploration("bash"));
        assert!(!r.is_read_only("bash"));
        assert!(!r.is_never_restrict("bash"));
        assert!(!r.is_parallelizable("bash"));
        assert!(!r.is_compactable("bash"));
    }

    #[test]
    fn write_file_is_mutating_approval_required() {
        let r = registry();
        assert_eq!(r.category("write_file"), ToolCategory::Mutating);
        assert!(r.is_approval_required("write_file"));
        assert!(!r.is_read_only("write_file"));
        assert!(!r.is_compactable("write_file"));
    }

    #[test]
    fn str_replace_is_mutating() {
        let r = registry();
        assert_eq!(r.category("str_replace"), ToolCategory::Mutating);
        assert!(!r.is_never_restrict("str_replace"));
    }

    #[test]
    fn skill_is_consultative_and_exploration_or_consultative() {
        let r = registry();
        assert!(r.is_consultative("skill"));
        assert!(r.is_exploration_or_consultative("skill"));
        assert!(!r.is_read_only("skill"));
        assert!(!r.is_compactable("skill"));
        assert!(!r.is_exploration("skill"));
    }

    #[test]
    fn git_read_tools_are_read_only_and_compactable() {
        let r = registry();
        for name in ["git_status", "git_diff", "git_log", "git_show", "git_blame"] {
            assert!(r.is_read_only(name), "{name} should be read-only");
            assert!(r.is_compactable(name), "{name} should be compactable");
            assert!(r.is_git_read(name), "{name} should be git-read");
            assert!(r.is_never_restrict(name), "{name} should be never-restrict");
        }
    }

    #[test]
    fn code_intel_tools_are_read_only_and_code_intel() {
        let r = registry();
        for name in [
            "symbols",
            "find_definition",
            "find_references",
            "symbol_search",
            "hover_info",
            "call_graph",
        ] {
            assert!(r.is_read_only(name), "{name} should be read-only");
            assert!(r.is_code_intel(name), "{name} should be code-intel");
            assert!(r.is_compactable(name), "{name} should be compactable");
        }
    }

    #[test]
    fn github_read_tools_are_read_only() {
        let r = registry();
        for name in [
            "github_list_prs",
            "github_get_pr",
            "github_ci_status",
            "github_list_issues",
            "github_get_issue",
            "github_repo_stats",
        ] {
            assert!(r.is_read_only(name), "{name} should be read-only");
            assert!(r.is_github_read(name), "{name} should be github-read");
        }
    }

    #[test]
    fn web_tools_are_read_only_and_compactable() {
        let r = registry();
        assert!(r.is_read_only("web_fetch"));
        assert!(r.is_compactable("web_fetch"));
        assert!(r.is_read_only("web_search"));
        assert!(r.is_compactable("web_search"));
    }

    #[test]
    fn memory_read_tools_are_read_only_but_not_compactable() {
        let r = registry();
        assert!(r.is_read_only("memory_search"));
        assert!(!r.is_compactable("memory_search"));
        assert!(r.is_read_only("memory_retrieve"));
        assert!(!r.is_compactable("memory_retrieve"));
    }

    #[test]
    fn memory_write_tools_are_mutating() {
        let r = registry();
        for name in ["memory_store", "memory_correct", "memory_purge"] {
            assert_eq!(r.category(name), ToolCategory::Mutating, "{name}");
        }
    }

    #[test]
    fn unknown_tool_defaults_to_mutating_no_flags() {
        let r = registry();
        assert_eq!(r.category("unknown_tool_xyz"), ToolCategory::Mutating);
        assert!(!r.is_read_only("unknown_tool_xyz"));
        assert!(!r.is_compactable("unknown_tool_xyz"));
        assert!(!r.is_approval_required("unknown_tool_xyz"));
    }

    #[test]
    fn aliases_share_category_with_canonical() {
        let r = registry();
        assert_eq!(r.category("read_file"), r.category("file_read"));
        assert_eq!(r.category("read_file"), r.category("ReadFileTool"));
        assert_eq!(r.category("bash"), r.category("BashTool"));
        assert_eq!(r.category("write_file"), r.category("WriteFileTool"));
        assert!(r.is_alias("file_read"));
        assert!(r.is_alias("ReadFileTool"));
        assert!(!r.is_alias("read_file"));
        assert!(!r.is_alias("bash"));
    }

    #[test]
    fn execute_command_names_are_subset_of_approval_required() {
        let r = registry();
        let approval = r.approval_required_names();
        for name in r.execute_command_names() {
            assert!(
                approval.contains(&name),
                "{name} is execute_command but not approval_required"
            );
        }
    }

    #[test]
    fn shell_names_match_execute_command_names() {
        let r = registry();
        let shells: std::collections::HashSet<_> = r.shell_names().into_iter().collect();
        let execs: std::collections::HashSet<_> = r.execute_command_names().into_iter().collect();
        assert_eq!(shells, execs);
    }

    #[test]
    fn read_only_names_superset_of_old_never_restrict() {
        let r = registry();
        let ro_names = r.read_only_names();
        for name in [
            "read_file",
            "list_dir",
            "grep",
            "glob",
            "git_status",
            "git_diff",
            "git_show",
            "git_log",
        ] {
            assert!(
                ro_names.contains(&name),
                "{name} from old READ_ONLY_NEVER_RESTRICT not in read_only_names"
            );
        }
    }

    #[test]
    fn compactable_names_superset_of_old_compactable_tools() {
        let r = registry();
        let compactable = r.compactable_names();
        for name in [
            "read_file",
            "grep",
            "glob",
            "list_dir",
            "git_show",
            "git_diff",
            "git_log",
            "git_status",
            "git_blame",
            "web_search",
            "web_fetch",
            "symbols",
            "find_definition",
            "find_references",
        ] {
            assert!(
                compactable.contains(&name),
                "{name} from old COMPACTABLE_TOOLS not in compactable_names"
            );
        }
    }

    #[test]
    fn headless_read_only_excludes_aliases_web_memory() {
        let r = registry();
        let headless = r.headless_read_only_names();
        assert!(headless.contains(&"read_file"));
        assert!(headless.contains(&"git_status"));
        assert!(headless.contains(&"symbols"));
        assert!(headless.contains(&"github_list_prs"));
        assert!(headless.contains(&"get_agent_info"));
        assert!(!headless.contains(&"file_read"));
        assert!(!headless.contains(&"ReadFileTool"));
        assert!(!headless.contains(&"web_fetch"));
        assert!(!headless.contains(&"web_search"));
        assert!(!headless.contains(&"memory_search"));
    }

    #[test]
    fn category_ordering_read_only_less_than_shell() {
        assert!(ToolCategory::ReadOnly < ToolCategory::Consultative);
        assert!(ToolCategory::Consultative < ToolCategory::Mutating);
        assert!(ToolCategory::Mutating < ToolCategory::Shell);
    }

    #[test]
    fn no_duplicate_names_in_table() {
        let mut seen = std::collections::HashSet::new();
        for meta in TOOL_TABLE {
            assert!(
                seen.insert(meta.name),
                "duplicate tool name in TOOL_TABLE: {}",
                meta.name
            );
        }
    }

    #[test]
    fn exploration_names_match_old_exploration_tools() {
        let r = registry();
        let expl = r.exploration_names();
        for name in [
            "bash",
            "list_dir",
            "read_file",
            "glob",
            "grep",
            "symbol_search",
            "hover_info",
            "call_graph",
            "find_definition",
            "find_references",
            "symbols",
        ] {
            assert!(expl.contains(&name), "{name} should be exploration");
        }
    }

    #[test]
    fn old_cloud_approval_required_tools_all_flagged() {
        let r = registry();
        for name in [
            "bash",
            "create_file",
            "delete_file",
            "edit_file",
            "exec",
            "git_commit",
            "git_revert_commit",
            "git_stash",
            "github_create_issue",
            "multi_edit",
            "rollback_database_snapshots",
            "rollback_file_edits",
            "rollback_turn_actions",
            "run_command",
            "shell",
            "str_replace",
            "write_file",
        ] {
            assert!(
                r.is_approval_required(name),
                "{name} should be approval_required"
            );
        }
    }

    #[test]
    fn old_shell_execution_tools_all_flagged() {
        let r = registry();
        for name in ["bash", "exec", "run_command", "shell"] {
            assert!(r.is_shell(name), "{name} should be shell");
            assert!(
                r.is_execute_command(name),
                "{name} should be execute_command"
            );
        }
    }

    #[test]
    fn old_parallel_read_only_tools_all_parallelizable() {
        let r = registry();
        for name in [
            "read_file",
            "file_read",
            "ReadFileTool",
            "grep",
            "GrepTool",
            "glob",
            "GlobTool",
            "list_dir",
            "ListDirTool",
            "web_fetch",
            "WebFetchTool",
            "web_search",
            "WebSearchTool",
            "memory_search",
            "memory_retrieve",
            "get_file_contents",
            "search_code",
            "list_files",
            "find_files",
            "view_file",
            "git_status",
            "git_diff",
            "git_log",
            "git_show",
            "git_blame",
            "find_definition",
            "find_references",
        ] {
            assert!(r.is_parallelizable(name), "{name} should be parallelizable");
        }
    }

    // ── Complex cross-system scenario tests ────────────────────────────

    /// Simulates a real agentic turn: the LLM emits a batch of tool calls
    /// during an investigation → edit → verify cycle. Validates that the
    /// registry produces correct partitioning, approval gating, compaction
    /// eligibility, and stall-detection classification for each phase.
    #[test]
    fn scenario_investigation_edit_verify_cycle() {
        let r = registry();

        // Phase 1: Investigation — all read-only, all parallelizable
        let investigation = [
            "read_file",
            "grep",
            "find_definition",
            "git_diff",
            "symbols",
        ];
        for name in investigation {
            assert!(
                r.is_parallelizable(name),
                "investigation tool {name} must be parallelizable"
            );
            assert!(
                r.is_never_restrict(name),
                "investigation tool {name} must never be restricted"
            );
            assert!(
                r.is_compactable(name),
                "investigation tool {name} should be compactable"
            );
            assert!(
                !r.is_approval_required(name),
                "investigation tool {name} must not need approval"
            );
        }

        // Phase 2: Edit — all mutating, all need approval, none parallelizable
        let edits = ["str_replace", "write_file", "create_file"];
        for name in edits {
            assert!(
                !r.is_parallelizable(name),
                "edit tool {name} must NOT be parallelizable"
            );
            assert!(
                r.is_approval_required(name),
                "edit tool {name} must need approval"
            );
            assert!(
                !r.is_compactable(name),
                "edit tool {name} must NOT be compactable"
            );
            assert!(
                !r.is_never_restrict(name),
                "edit tool {name} must be restrictable"
            );
        }

        // Phase 3: Verification — back to read-only
        let verify = ["read_file", "git_status", "git_diff"];
        for name in verify {
            assert!(r.is_parallelizable(name));
            assert!(r.is_never_restrict(name));
        }
    }

    /// A batch containing a mix of read-only and mutating tools:
    /// the parallel executor should separate them correctly.
    #[test]
    fn scenario_mixed_batch_partitioning() {
        let r = registry();
        let batch = [
            ("read_file", true),
            ("grep", true),
            ("write_file", false),
            ("git_status", true),
            ("bash", false),
            ("find_definition", true),
            ("delete_file", false),
            ("memory_retrieve", true),
        ];
        for (name, expect_parallel) in batch {
            assert_eq!(
                r.is_parallelizable(name),
                expect_parallel,
                "{name}: expected parallelizable={expect_parallel}"
            );
        }
    }

    /// Stall detector scenario: 5 rounds of pure exploration tools should
    /// all be classified as exploration, while consultative tools should
    /// only trigger the broader is_exploration_or_consultative check.
    #[test]
    fn scenario_stall_exploration_detection() {
        let r = registry();

        // Pure exploration round — stall detector sees these
        let exploration_round = ["read_file", "grep", "list_dir", "symbols", "bash"];
        for name in exploration_round {
            assert!(r.is_exploration(name), "{name} should be exploration");
        }

        // Consultative round — triggers broader check only
        let consultative_round = ["skill", "discover_skills"];
        for name in consultative_round {
            assert!(
                !r.is_exploration(name),
                "{name} is consultative, not exploration"
            );
            assert!(
                r.is_exploration_or_consultative(name),
                "{name} must be detected by broader check"
            );
        }

        // Mutating tools break an exploration chain
        for name in ["write_file", "str_replace", "multi_edit"] {
            assert!(!r.is_exploration(name), "{name} must NOT be exploration");
            assert!(!r.is_exploration_or_consultative(name));
        }
    }

    /// Cloud deployment scenario: headless mode gets a restricted tool set
    /// (no web, no memory, no aliases), while cloud approval gates all
    /// mutating + shell tools. MCP tools (prefix mcp_) are unknown to the
    /// registry and get fail-closed defaults.
    #[test]
    fn scenario_cloud_headless_deployment() {
        let r = registry();
        let headless = r.headless_read_only_names();

        // Headless set includes core investigation tools
        assert!(headless.contains(&"read_file"));
        assert!(headless.contains(&"grep"));
        assert!(headless.contains(&"git_status"));
        assert!(headless.contains(&"find_definition"));
        assert!(headless.contains(&"github_list_prs"));

        // Headless set excludes web (needs network), memory (needs server),
        // MatrixOne (needs DB), orchestration (agent-internal), aliases
        assert!(!headless.contains(&"web_fetch"));
        assert!(!headless.contains(&"memory_search"));
        assert!(!headless.contains(&"mo_query"));
        assert!(!headless.contains(&"context_analysis"));
        assert!(!headless.contains(&"ReadFileTool"));
        assert!(!headless.contains(&"GrepTool"));

        // Every tool in headless set is read-only and compactable
        for name in &headless {
            assert!(
                r.is_read_only(name),
                "headless tool {name} must be read-only"
            );
            assert!(
                r.is_compactable(name),
                "headless tool {name} must be compactable"
            );
            assert!(
                !r.is_approval_required(name),
                "headless tool {name} must not need approval"
            );
        }

        // MCP tools are unknown → fail-closed
        let mcp_tool = "mcp_custom_server_query";
        assert_eq!(r.category(mcp_tool), ToolCategory::Mutating);
        assert!(!r.is_parallelizable(mcp_tool));
        assert!(!r.is_compactable(mcp_tool));
    }

    /// Concurrency safety registry is empty by default (MCP-only).
    /// Static tools are classified through tool_categories::classify(),
    /// which is authoritative. Verify the chain:
    ///   tool_categories → parallel_tool_exec::is_read_only_tool
    #[test]
    fn scenario_concurrency_bootstrap_chain() {
        let r = registry();
        let cs = crate::concurrency_safety::ConcurrencySafetyRegistry::bootstrap_default();

        // bootstrap_default is empty — static tools are NOT in the registry
        assert!(cs.is_empty());

        // Read-only tools: classified via tool_categories, surfaced via
        // parallel_tool_exec::is_read_only_tool (which delegates to classify).
        for name in ["read_file", "grep", "glob", "git_status", "find_definition"] {
            assert!(r.is_parallelizable(name), "{name} should be parallelizable");
            assert!(
                crate::parallel_tool_exec::is_read_only_tool(name),
                "{name} should be read-only via parallel_tool_exec"
            );
            // Concurrency registry returns Unknown for static tools (expected)
            assert_eq!(
                cs.classify(name),
                crate::concurrency_safety::ConcurrencySafety::Unknown,
                "{name} should be Unknown in empty concurrency registry"
            );
        }

        // Shell tools: classified via tool_categories
        for name in ["bash", "exec", "run_command", "shell"] {
            assert!(r.is_shell(name), "{name} should be shell");
            assert!(
                !crate::parallel_tool_exec::is_read_only_tool(name),
                "{name} should NOT be read-only without args"
            );
        }

        // Mutating tools
        for name in ["write_file", "edit_file", "str_replace", "delete_file"] {
            assert_eq!(r.category(name), ToolCategory::Mutating);
            assert!(
                !crate::parallel_tool_exec::is_read_only_tool(name),
                "{name} should NOT be read-only"
            );
        }

        // Unknown tool: both agree on safe defaults
        let unknown = "brand_new_mcp_tool";
        assert_eq!(r.category(unknown), ToolCategory::Mutating);
        assert_eq!(
            cs.classify(unknown),
            crate::concurrency_safety::ConcurrencySafety::Unknown,
        );
    }

    /// Alias consistency: every alias must have identical classification
    /// to its canonical tool across ALL dimensions.
    #[test]
    fn scenario_alias_full_consistency() {
        let r = registry();
        let alias_pairs = [
            ("read_file", "file_read"),
            ("read_file", "ReadFileTool"),
            ("grep", "GrepTool"),
            ("glob", "GlobTool"),
            ("list_dir", "ListDirTool"),
            ("write_file", "WriteFileTool"),
            ("edit_file", "EditFileTool"),
            ("bash", "BashTool"),
            ("web_fetch", "WebFetchTool"),
            ("web_search", "WebSearchTool"),
            ("apply_patch", "ApplyPatchTool"),
            ("powershell", "PowerShellTool"),
        ];
        for (canonical, alias) in alias_pairs {
            assert_eq!(
                r.category(canonical),
                r.category(alias),
                "category mismatch: {canonical} vs {alias}"
            );
            assert!(r.is_alias(alias), "{alias} should be flagged as alias");
            assert!(!r.is_alias(canonical), "{canonical} should NOT be an alias");

            // Approval-required must match (modulo ALIAS flag filtering in lists)
            assert_eq!(
                r.is_approval_required(canonical),
                r.is_approval_required(alias),
                "approval mismatch: {canonical} vs {alias}"
            );
            assert_eq!(
                r.is_parallelizable(canonical),
                r.is_parallelizable(alias),
                "parallelizable mismatch: {canonical} vs {alias}"
            );
        }
    }

    /// Invariant: every tool in the table with APPROVAL_REQUIRED must be
    /// either Mutating or Shell (never ReadOnly or Consultative).
    #[test]
    fn invariant_approval_only_on_mutating_or_shell() {
        for meta in TOOL_TABLE {
            if meta.flags.contains(ToolFlags::APPROVAL_REQUIRED) {
                assert!(
                    meta.category.is_mutating() || meta.category.is_shell(),
                    "{} has APPROVAL_REQUIRED but category {:?} — read-only and consultative tools must not require approval",
                    meta.name,
                    meta.category,
                );
            }
        }
    }

    /// Invariant: EXECUTE_COMMAND implies APPROVAL_REQUIRED.
    #[test]
    fn invariant_execute_implies_approval() {
        for meta in TOOL_TABLE {
            if meta.flags.contains(ToolFlags::EXECUTE_COMMAND) {
                assert!(
                    meta.flags.contains(ToolFlags::APPROVAL_REQUIRED),
                    "{} has EXECUTE_COMMAND but not APPROVAL_REQUIRED",
                    meta.name,
                );
            }
        }
    }

    /// Invariant: ALIAS tools must not appear in approval_required_names
    /// or execute_command_names (avoid double-counting in cloud gating).
    #[test]
    fn invariant_aliases_excluded_from_approval_lists() {
        let r = registry();
        for name in r.approval_required_names() {
            assert!(
                !r.is_alias(name),
                "{name} is an alias in approval_required_names"
            );
        }
        for name in r.execute_command_names() {
            assert!(
                !r.is_alias(name),
                "{name} is an alias in execute_command_names"
            );
        }
        for name in r.shell_names() {
            assert!(!r.is_alias(name), "{name} is an alias in shell_names");
        }
    }

    // ════════════════════════════════════════════════════════════════════
    // ToolClassification + classify() tests (TDD — args-aware)
    // ════════════════════════════════════════════════════════════════════

    use serde_json::json;

    // ── Basic name-only classification ──────────────────────────────

    #[test]
    fn classify_read_file_no_args() {
        let c = classify_name("read_file");
        assert_eq!(c.category, ToolCategory::ReadOnly);
        assert!(c.parallelizable);
        assert!(!c.approval_required);
        assert!(c.compactable);
        assert!(c.never_restrict);
        assert!(c.exploration);
    }

    #[test]
    fn classify_write_file_no_args() {
        let c = classify_name("write_file");
        assert_eq!(c.category, ToolCategory::Mutating);
        assert!(!c.parallelizable);
        assert!(c.approval_required);
        assert!(!c.compactable);
        assert!(!c.never_restrict);
        assert!(!c.exploration);
    }

    #[test]
    fn classify_bash_no_args_is_shell() {
        let c = classify_name("bash");
        assert_eq!(c.category, ToolCategory::Shell);
        assert!(!c.parallelizable);
        assert!(c.approval_required);
        assert!(!c.compactable);
        assert!(!c.never_restrict);
        assert!(c.exploration);
    }

    #[test]
    fn classify_skill_is_consultative_and_parallelizable() {
        let c = classify_name("skill");
        assert_eq!(c.category, ToolCategory::Consultative);
        assert!(
            c.parallelizable,
            "consultative tools must be parallelizable"
        );
        assert!(!c.approval_required);
        assert!(
            !c.never_restrict,
            "consultative tools must be restrictable for stall avoidance"
        );
        assert!(c.exploration, "consultative tools count as exploration");
    }

    #[test]
    fn classify_unknown_tool_fail_closed() {
        let c = classify_name("mcp_unknown_server_tool");
        assert_eq!(c.category, ToolCategory::Mutating);
        assert!(!c.parallelizable);
        assert!(!c.approval_required);
        assert!(!c.compactable);
        assert!(!c.never_restrict);
        assert!(!c.exploration);
    }

    // ── Args-aware bash classification (the killer feature) ─────────

    #[test]
    fn classify_bash_git_status_is_read_only() {
        let args = json!({"command": "git status"});
        let c = classify("bash", Some(&args));
        assert_eq!(
            c.category,
            ToolCategory::ReadOnly,
            "bash 'git status' should be classified as ReadOnly"
        );
        assert!(
            c.parallelizable,
            "bash 'git status' should be parallelizable"
        );
        assert!(
            !c.approval_required,
            "bash 'git status' should NOT need approval"
        );
        assert!(
            c.compactable,
            "bash 'git status' result should be compactable"
        );
        assert!(c.exploration, "bash 'git status' is exploration");
    }

    #[test]
    fn classify_bash_ls_is_read_only() {
        let args = json!({"command": "ls -la"});
        let c = classify("bash", Some(&args));
        assert!(c.parallelizable);
        assert!(!c.approval_required);
        assert_eq!(c.category, ToolCategory::ReadOnly);
    }

    #[test]
    fn classify_bash_cargo_check_is_read_only() {
        let args = json!({"command": "cargo check 2>&1 | head -50"});
        let c = classify("bash", Some(&args));
        assert!(c.parallelizable);
        assert!(!c.approval_required);
    }

    #[test]
    fn classify_bash_grep_is_read_only() {
        let args = json!({"command": "grep -r pattern ."});
        let c = classify("bash", Some(&args));
        assert!(c.parallelizable);
        assert!(!c.approval_required);
    }

    #[test]
    fn classify_bash_cd_and_ls_is_read_only() {
        let args = json!({"command": "cd project && ls"});
        let c = classify("bash", Some(&args));
        assert!(c.parallelizable);
        assert!(!c.approval_required);
    }

    #[test]
    fn classify_bash_rm_is_mutating() {
        let args = json!({"command": "rm -rf /tmp/trash"});
        let c = classify("bash", Some(&args));
        assert_eq!(
            c.category,
            ToolCategory::Shell,
            "bash 'rm' should stay Shell"
        );
        assert!(!c.parallelizable, "bash 'rm' must NOT be parallelizable");
        assert!(c.approval_required, "bash 'rm' must need approval");
    }

    #[test]
    fn classify_bash_git_push_is_mutating() {
        let args = json!({"command": "git push origin main"});
        let c = classify("bash", Some(&args));
        assert!(!c.parallelizable);
        assert!(c.approval_required);
    }

    #[test]
    fn classify_bash_cargo_build_is_mutating() {
        let args = json!({"command": "cargo build"});
        let c = classify("bash", Some(&args));
        assert!(!c.parallelizable);
        assert!(c.approval_required);
    }

    #[test]
    fn classify_bash_pip_install_is_mutating() {
        let args = json!({"command": "pip install requests"});
        let c = classify("bash", Some(&args));
        assert!(!c.parallelizable);
        assert!(c.approval_required);
    }

    #[test]
    fn classify_bash_empty_command_is_not_read_only() {
        let args = json!({"command": ""});
        let c = classify("bash", Some(&args));
        assert!(!c.parallelizable);
        assert!(c.approval_required);
    }

    #[test]
    fn classify_bash_output_redirect_is_mutating() {
        let args = json!({"command": "ls > output.txt"});
        let c = classify("bash", Some(&args));
        assert!(!c.parallelizable);
        assert!(c.approval_required);
    }

    #[test]
    fn classify_bash_no_command_arg_is_shell() {
        let args = json!({"other_field": "irrelevant"});
        let c = classify("bash", Some(&args));
        assert_eq!(c.category, ToolCategory::Shell);
        assert!(!c.parallelizable);
        assert!(c.approval_required);
    }

    // ── BashTool alias should also get args-aware classification ─────

    #[test]
    fn classify_bashtool_alias_git_status() {
        let args = json!({"command": "git status"});
        let c = classify("BashTool", Some(&args));
        assert_eq!(c.category, ToolCategory::ReadOnly);
        assert!(c.parallelizable);
        assert!(!c.approval_required);
    }

    // ── Non-shell tools ignore args ─────────────────────────────────

    #[test]
    fn classify_read_file_with_args_unchanged() {
        let args = json!({"path": "/etc/passwd"});
        let c = classify("read_file", Some(&args));
        assert_eq!(c.category, ToolCategory::ReadOnly);
        assert!(c.parallelizable);
        assert!(!c.approval_required);
    }

    #[test]
    fn classify_write_file_with_args_still_mutating() {
        let args = json!({"path": "test.txt", "content": "hello"});
        let c = classify("write_file", Some(&args));
        assert_eq!(c.category, ToolCategory::Mutating);
        assert!(c.approval_required);
    }

    // ── Scenario: mixed bash batch (the cloud-edge advantage) ───────

    #[test]
    fn scenario_mixed_bash_batch_args_aware() {
        let batch = [
            (json!({"command": "git status"}), true, false),
            (json!({"command": "grep -r TODO ."}), true, false),
            (json!({"command": "cargo check 2>&1"}), true, false),
            (json!({"command": "cargo build"}), false, true),
            (json!({"command": "git push"}), false, true),
            (json!({"command": "rm temp.txt"}), false, true),
        ];
        for (args, expect_parallel, expect_approval) in &batch {
            let c = classify("bash", Some(args));
            let cmd = args["command"].as_str().unwrap();
            assert_eq!(
                c.parallelizable, *expect_parallel,
                "bash '{cmd}': expected parallelizable={expect_parallel}"
            );
            assert_eq!(
                c.approval_required, *expect_approval,
                "bash '{cmd}': expected approval={expect_approval}"
            );
        }
    }

    /// Full agentic turn with bash-heavy investigation:
    /// 5 read-only bash commands run in parallel, then one mutating
    /// bash command runs sequentially.
    #[test]
    fn scenario_agentic_turn_bash_investigation_then_edit() {
        // Phase 1: Investigation — all bash read-only, all parallelizable
        let investigation_cmds = [
            "git status",
            "git diff HEAD",
            "grep -r 'fn main' .",
            "cat Cargo.toml",
            "find . -name '*.rs'",
        ];
        for cmd in investigation_cmds {
            let c = classify("bash", Some(&json!({"command": cmd})));
            assert!(
                c.parallelizable,
                "bash '{cmd}' should be parallelizable in investigation phase"
            );
            assert!(
                !c.approval_required,
                "bash '{cmd}' should not need approval"
            );
        }

        // Phase 2: Edit via str_replace (not bash)
        let c = classify("str_replace", None);
        assert!(!c.parallelizable);
        assert!(c.approval_required);

        // Phase 3: Verify — bash read-only again
        let c = classify("bash", Some(&json!({"command": "git diff"})));
        assert!(c.parallelizable);
        assert!(!c.approval_required);
    }

    /// Cloud edge advantage: headless mode can skip approval round-trip
    /// for read-only bash commands, saving ~200ms per call.
    #[test]
    fn scenario_cloud_approval_bypass_for_read_only_bash() {
        let read_only_bash = [
            "git log --oneline -10",
            "ls -la src/",
            "cargo clippy 2>&1 | head -20",
            "npm list",
        ];
        for cmd in read_only_bash {
            let c = classify("bash", Some(&json!({"command": cmd})));
            assert!(
                !c.approval_required,
                "bash '{cmd}' should bypass cloud approval"
            );
            assert!(c.parallelizable, "bash '{cmd}' should run in parallel");
        }

        let mutating_bash = ["cargo build", "npm install", "git commit -m 'x'"];
        for cmd in mutating_bash {
            let c = classify("bash", Some(&json!({"command": cmd})));
            assert!(
                c.approval_required,
                "bash '{cmd}' must require cloud approval"
            );
        }
    }

    /// Consultative tools (skill, discover_skills) should be
    /// parallelizable — they don't mutate anything.
    #[test]
    fn scenario_consultative_parallelizable() {
        for name in ["skill", "discover_skills"] {
            let c = classify_name(name);
            assert!(c.parallelizable, "{name} must be parallelizable");
            assert!(
                !c.never_restrict,
                "{name} must be restrictable for stall avoidance"
            );
            assert!(!c.approval_required);
        }
    }

    /// classify_name and classify(name, None) must be identical.
    #[test]
    fn classify_name_equals_classify_none() {
        for name in ["read_file", "bash", "write_file", "skill", "unknown"] {
            let a = classify_name(name);
            let b = classify(name, None);
            assert_eq!(a, b, "classify_name vs classify(_, None) for {name}");
        }
    }

    // ════════════════════════════════════════════════════════════════════
    // Idempotency tests
    // ════════════════════════════════════════════════════════════════════

    #[test]
    fn idempotency_read_only_tools_are_pure_read() {
        let r = registry();
        for name in [
            "read_file",
            "grep",
            "glob",
            "list_dir",
            "git_status",
            "git_log",
            "git_diff",
            "git_blame",
            "git_file_history",
            "git_contributors",
            "git_log_search",
            "github_list_prs",
            "github_get_pr",
            "github_list_issues",
            "github_get_issue",
            "github_ci_status",
            "github_repo_stats",
            "memory_search",
            "memory_profile",
            "web_fetch",
            "get_agent_info",
        ] {
            assert_eq!(
                r.idempotency(name),
                ToolIdempotency::PureRead,
                "{name} should be PureRead"
            );
        }
    }

    #[test]
    fn idempotency_consultative_tools_are_pure_read() {
        let r = registry();
        for name in ["skill", "discover_skills"] {
            assert_eq!(
                r.idempotency(name),
                ToolIdempotency::PureRead,
                "{name} should be PureRead"
            );
        }
    }

    #[test]
    fn idempotency_write_file_is_idempotent_write() {
        let r = registry();
        assert_eq!(
            r.idempotency("write_file"),
            ToolIdempotency::IdempotentWrite
        );
        assert_eq!(
            r.idempotency("WriteFileTool"),
            ToolIdempotency::IdempotentWrite
        );
    }

    #[test]
    fn idempotency_mutating_tools_are_non_idempotent() {
        let r = registry();
        for name in [
            "bash",
            "str_replace",
            "github_create_issue",
            "memory_store",
            "memory_purge",
            "memory_correct",
            "delete_file",
            "multi_edit",
            "edit_file",
            "git_commit",
            "git_revert_commit",
            "git_stash",
        ] {
            assert_eq!(
                r.idempotency(name),
                ToolIdempotency::NonIdempotent,
                "{name} should be NonIdempotent"
            );
        }
    }

    #[test]
    fn idempotency_unknown_tool_defaults_to_non_idempotent() {
        let r = registry();
        assert_eq!(
            r.idempotency("some_future_tool"),
            ToolIdempotency::NonIdempotent
        );
    }

    #[test]
    fn idempotency_invariant_read_only_category_implies_pure_read() {
        for meta in TOOL_TABLE {
            if meta.category == ToolCategory::ReadOnly {
                assert_eq!(
                    meta.idempotency,
                    ToolIdempotency::PureRead,
                    "{} is ReadOnly but not PureRead",
                    meta.name,
                );
            }
        }
    }

    #[test]
    fn idempotency_invariant_shell_never_pure_read() {
        for meta in TOOL_TABLE {
            if meta.category == ToolCategory::Shell {
                assert_ne!(
                    meta.idempotency,
                    ToolIdempotency::PureRead,
                    "{} is Shell but PureRead",
                    meta.name,
                );
            }
        }
    }

    #[test]
    fn classify_idempotency_matches_registry() {
        let r = registry();
        for meta in TOOL_TABLE {
            let c = classify_name(meta.name);
            assert_eq!(
                c.idempotency,
                r.idempotency(meta.name),
                "classify and registry disagree on idempotency for {}",
                meta.name,
            );
        }
    }

    #[test]
    fn classify_bash_read_only_command_has_pure_read_idempotency() {
        let args = json!({"command": "git status"});
        let c = classify("bash", Some(&args));
        assert_eq!(c.idempotency, ToolIdempotency::PureRead);
    }

    #[test]
    fn classify_bash_mutating_command_has_non_idempotent() {
        let args = json!({"command": "cargo build"});
        let c = classify("bash", Some(&args));
        assert_eq!(c.idempotency, ToolIdempotency::NonIdempotent);
    }

    /// Cross-system scenario: retry policy derivation chain.
    /// Read-only tools → aggressive retry, write_file → cautious,
    /// bash/str_replace → no retry.
    #[test]
    fn scenario_retry_policy_derivation() {
        let r = registry();

        let pure_reads = ["read_file", "grep", "git_status", "memory_search"];
        for name in pure_reads {
            let idem = r.idempotency(name);
            assert!(
                idem.is_safe_to_retry(),
                "{name}: PureRead should be safe to retry"
            );
            assert!(idem.is_pure_read(), "{name}: should be pure read");
        }

        let idempotent_write = "write_file";
        let idem = r.idempotency(idempotent_write);
        assert!(
            idem.is_safe_to_retry(),
            "write_file: IdempotentWrite should be safe to retry"
        );
        assert!(!idem.is_pure_read(), "write_file: should NOT be pure read");

        let non_idempotent = ["bash", "str_replace", "github_create_issue"];
        for name in non_idempotent {
            let idem = r.idempotency(name);
            assert!(
                !idem.is_safe_to_retry(),
                "{name}: NonIdempotent should NOT be safe to retry"
            );
        }
    }

    /// Full consistency: every tool in step_protocol's old hardcoded lists
    /// maps to the correct idempotency in the central registry.
    #[test]
    fn scenario_step_protocol_compatibility() {
        let r = registry();

        let old_pure_reads = [
            "read_file",
            "grep",
            "glob",
            "list_dir",
            "git_status",
            "git_log",
            "git_diff",
            "git_blame",
            "git_file_history",
            "git_contributors",
            "git_log_search",
            "github_list_prs",
            "github_get_pr",
            "github_list_issues",
            "github_get_issue",
            "github_ci_status",
            "github_repo_stats",
            "memory_search",
            "memory_profile",
            "web_fetch",
            "get_agent_info",
        ];
        for name in old_pure_reads {
            assert_eq!(
                r.idempotency(name),
                ToolIdempotency::PureRead,
                "step_protocol compat: {name} should be PureRead"
            );
        }

        assert_eq!(
            r.idempotency("write_file"),
            ToolIdempotency::IdempotentWrite,
            "step_protocol compat: write_file should be IdempotentWrite"
        );

        let old_non_idempotent = [
            "bash",
            "str_replace",
            "github_create_issue",
            "memory_store",
            "memory_purge",
            "memory_correct",
        ];
        for name in old_non_idempotent {
            assert_eq!(
                r.idempotency(name),
                ToolIdempotency::NonIdempotent,
                "step_protocol compat: {name} should be NonIdempotent"
            );
        }
    }

    /// Every tool in TOOL_TABLE must agree with the canonical
    /// classify_tool_idempotency function from astra-turn-types.
    #[test]
    fn invariant_table_idempotency_matches_shared_classifier() {
        for meta in TOOL_TABLE {
            assert_eq!(
                meta.idempotency,
                classify_tool_idempotency(meta.name),
                "TOOL_TABLE idempotency for {} disagrees with shared classify_tool_idempotency",
                meta.name,
            );
        }
    }

    // ── Display category tests ──────────────────────────────────────

    #[test]
    fn display_category_github_tools() {
        let r = registry();
        for name in [
            "github_list_prs",
            "github_get_pr",
            "github_ci_status",
            "github_list_issues",
            "github_get_issue",
            "github_repo_stats",
            "github_create_issue",
        ] {
            assert_eq!(
                r.display_category(name),
                ToolDisplayCategory::Github,
                "{name} should be Github category"
            );
        }
    }

    #[test]
    fn display_category_file_tools() {
        let r = registry();
        for name in [
            "read_file",
            "write_file",
            "str_replace",
            "multi_edit",
            "edit_file",
            "create_file",
            "delete_file",
            "apply_patch",
            "view_file",
        ] {
            assert_eq!(
                r.display_category(name),
                ToolDisplayCategory::File,
                "{name} should be File category"
            );
        }
    }

    #[test]
    fn display_category_shell_tools() {
        let r = registry();
        for name in [
            "bash",
            "exec",
            "run_command",
            "shell",
            "powershell",
            "run_build_test",
        ] {
            assert_eq!(
                r.display_category(name),
                ToolDisplayCategory::Shell,
                "{name} should be Shell category"
            );
        }
    }

    #[test]
    fn display_category_search_tools() {
        let r = registry();
        for name in [
            "search",
            "grep",
            "find",
            "glob",
            "list_dir",
            "tool_search",
            "web_fetch",
            "web_search",
        ] {
            assert_eq!(
                r.display_category(name),
                ToolDisplayCategory::Search,
                "{name} should be Search category"
            );
        }
    }

    #[test]
    fn display_category_git_tools() {
        let r = registry();
        for name in [
            "git_status",
            "git_diff",
            "git_log",
            "git_show",
            "git_blame",
            "git_file_history",
            "git_contributors",
            "git_log_search",
            "git_commit",
            "git_revert_commit",
            "git_stash",
            "git_checkout_file",
            "git_worktree",
        ] {
            assert_eq!(
                r.display_category(name),
                ToolDisplayCategory::Git,
                "{name} should be Git category"
            );
        }
    }

    #[test]
    fn display_category_code_intel_tools() {
        let r = registry();
        for name in [
            "symbols",
            "find_definition",
            "find_references",
            "symbol_search",
            "hover_info",
            "call_graph",
            "type_hierarchy",
            "dead_code",
            "extract_members",
            "lsp",
            "rename_symbol",
        ] {
            assert_eq!(
                r.display_category(name),
                ToolDisplayCategory::Code,
                "{name} should be Code category"
            );
        }
    }

    #[test]
    fn display_category_mo_tools() {
        let r = registry();
        for name in ["mo_query", "mo_snapshot", "mo_branch"] {
            assert_eq!(
                r.display_category(name),
                ToolDisplayCategory::Mo,
                "{name} should be Mo category"
            );
        }
    }

    #[test]
    fn display_category_memory_tools() {
        let r = registry();
        for name in [
            "memory_search",
            "memory_retrieve",
            "memory_profile",
            "memory_store",
            "memory_correct",
            "memory_purge",
        ] {
            assert_eq!(
                r.display_category(name),
                ToolDisplayCategory::Memory,
                "{name} should be Memory category"
            );
        }
    }

    #[test]
    fn display_category_utility_tools() {
        let r = registry();
        for name in [
            "ask_user",
            "sleep",
            "brief",
            "query_context",
            "send_message",
            "spawn_agent",
            "share_context",
            "run_chain",
            "config",
            "adjust_config",
            "set_goal",
            "compress_context",
            "env",
            "notebook_edit",
            "task_create",
            "task_update",
            "task_stop",
            "task_list",
            "task_get",
            "context_analysis",
            "diagnose",
            "rollback_file_edits",
            "rollback_database_snapshots",
            "rollback_turn_actions",
            "rollback_session_state",
            "prioritize_tool",
            "deprioritize_tool",
            "reflect",
            "get_agent_info",
            "skill",
            "discover_skills",
        ] {
            assert_eq!(
                r.display_category(name),
                ToolDisplayCategory::Utility,
                "{name} should be Utility category"
            );
        }
    }

    #[test]
    fn display_category_unknown_tool_is_other() {
        let r = registry();
        assert_eq!(
            r.display_category("some_unknown_tool_xyz"),
            ToolDisplayCategory::Other
        );
    }

    /// Every tool in TOOL_TABLE must map to a display category that is NOT Other
    /// (Other is only for unknown tools).
    #[test]
    fn invariant_all_registered_tools_have_known_display_category() {
        let r = registry();
        for meta in TOOL_TABLE {
            if meta.flags.contains(ToolFlags::ALIAS) {
                continue;
            }
            assert_ne!(
                r.display_category(meta.name),
                ToolDisplayCategory::Other,
                "Registered tool {} should not fall through to Other display category",
                meta.name,
            );
        }
    }

    // ── Fix verification tests ────────────────────────────────────

    /// apply_patch must require cloud approval like all other mutating file ops.
    #[test]
    fn apply_patch_requires_approval() {
        let r = registry();
        assert!(
            r.is_approval_required("apply_patch"),
            "apply_patch should require approval like other mutating file ops"
        );
        assert!(
            r.is_approval_required("ApplyPatchTool"),
            "ApplyPatchTool alias should also require approval"
        );
    }

    /// All shell-category tools must have EXPLORATION for stall detection.
    #[test]
    fn all_shell_tools_have_exploration_flag() {
        let r = registry();
        for name in ["bash", "exec", "run_command", "shell", "powershell"] {
            assert!(
                r.is_exploration(name),
                "{name} should have EXPLORATION flag for stall detection"
            );
        }
    }

    /// Shell tools other than bash also support args-aware read-only detection.
    #[test]
    fn shell_variants_support_args_aware_classification() {
        use serde_json::json;
        for name in ["exec", "run_command", "shell", "powershell"] {
            let ro_args = json!({"command": "git status"});
            let c = classify(name, Some(&ro_args));
            assert!(
                c.parallelizable,
                "{name} with 'git status' should be parallelizable"
            );
            assert!(
                !c.approval_required,
                "{name} with 'git status' should not need approval"
            );
            assert_eq!(
                c.idempotency,
                ToolIdempotency::PureRead,
                "{name} with 'git status' should be PureRead"
            );

            let mu_args = json!({"command": "rm -rf /"});
            let c = classify(name, Some(&mu_args));
            assert!(
                !c.parallelizable,
                "{name} with 'rm -rf' should NOT be parallelizable"
            );
            assert!(
                c.approval_required,
                "{name} with 'rm -rf' should need approval"
            );
            assert_eq!(
                c.idempotency,
                ToolIdempotency::NonIdempotent,
                "{name} with 'rm -rf' should be NonIdempotent"
            );
        }
    }

    /// Consultative tools are parallelizable but NOT never-restrict.
    /// never_restrict is only for ReadOnly (observation) tools.
    /// Consultative tools CAN be restricted to break stall loops.
    #[test]
    fn consultative_parallelizable_but_restrictable() {
        let r = registry();
        for name in ["skill", "discover_skills", "ask_user", "sleep"] {
            assert!(r.is_parallelizable(name), "{name} should be parallelizable");
            assert!(
                !r.is_never_restrict(name),
                "{name} should NOT be never-restrict (stall avoidance needs to restrict these)"
            );
        }
    }

    /// Display category invariant also holds for alias tools.
    #[test]
    fn display_category_aliases_resolve_same_as_canonical() {
        let r = registry();
        let alias_pairs = [
            ("read_file", "file_read"),
            ("read_file", "ReadFileTool"),
            ("grep", "GrepTool"),
            ("glob", "GlobTool"),
            ("list_dir", "ListDirTool"),
            ("write_file", "WriteFileTool"),
            ("edit_file", "EditFileTool"),
            ("apply_patch", "ApplyPatchTool"),
            ("web_fetch", "WebFetchTool"),
            ("web_search", "WebSearchTool"),
            ("bash", "BashTool"),
            ("powershell", "PowerShellTool"),
        ];
        for (canonical, alias) in alias_pairs {
            assert_eq!(
                r.display_category(canonical),
                r.display_category(alias),
                "{canonical} and {alias} should have the same display category"
            );
        }
    }

    /// Scenario: headless status display routes every registered tool to a
    /// formatter that can handle it (no panic, no missed branch).
    #[test]
    fn scenario_display_category_covers_all_registered_tools() {
        let r = registry();
        let mut seen = std::collections::HashMap::<ToolDisplayCategory, Vec<&str>>::new();
        for meta in TOOL_TABLE {
            if meta.flags.contains(ToolFlags::ALIAS) {
                continue;
            }
            let cat = r.display_category(meta.name);
            seen.entry(cat).or_default().push(meta.name);
        }
        let expected_categories = [
            ToolDisplayCategory::Github,
            ToolDisplayCategory::File,
            ToolDisplayCategory::Shell,
            ToolDisplayCategory::Search,
            ToolDisplayCategory::Git,
            ToolDisplayCategory::Code,
            ToolDisplayCategory::Mo,
            ToolDisplayCategory::Memory,
            ToolDisplayCategory::Utility,
        ];
        for cat in expected_categories {
            assert!(
                seen.contains_key(&cat),
                "Display category {cat:?} has no registered tools — \
                 either the table or display_category() logic is wrong"
            );
        }
    }
}
