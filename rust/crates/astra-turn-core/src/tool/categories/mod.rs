//! Single source of truth for tool behavioral metadata.
//!
//! Every hardcoded tool-name list in the codebase (stall.rs, turn_guard.rs,
//! parallel_tool_exec.rs, microcompact.rs, headless_tool_assembly.rs,
//! safety_middleware.rs, cloud_approval_policy.rs, concurrency_safety.rs)
//! should derive its answers from queries against this registry.

use std::collections::HashMap;
use std::sync::OnceLock;

pub mod surface;
pub mod workaround;

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
    pub const MATRIXONE: Self = Self(1 << 9);
    pub const ORCHESTRATION: Self = Self(1 << 10);
    pub const FILE_OP: Self = Self(1 << 11);
    pub const TASK_MGMT: Self = Self(1 << 12);

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
    tool("list_dir", RO, C.union(EX)),
    tool("grep", RO, C.union(EX)),
    tool("glob", RO, C.union(EX)),
    tool("search", RO, C.union(EX)),
    tool("find", RO, C.union(EX)),
    tool("tool_search", RO, C),
    // Consolidated git tool; action-aware classification below fails closed
    // when args are absent.
    tool("git", MU, A),
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
    // Consolidated GitHub tool; action-aware classification below fails
    // closed when args are absent.
    tool("github", MU, A),
    // ── Web (read-only, compactable) ─────────────────────────────────
    tool("web_fetch", RO, WB),
    tool("web_search", RO, WB),
    // ── Memory / retrieval (read-only but not compactable) ───────────
    tool("memory_search", RO, ME),
    tool("memory_retrieve", RO, ME),
    tool("memory_profile", RO, ME),
    tool("session_history_page", RO, ME),
    tool("session_history_search", RO, ME),
    tool("session_history_around", RO, ME),
    // ── Memory (action-aware) ────────────────────────────────────────
    //
    // Single consolidated entry. The `action` arg determines read vs write.
    // Base classification is Mutating + NonIdempotent (conservative default
    // when args are unavailable). Callers that have args use
    // `Registry::category_for(name, args)` / `idempotency_for(name, args)`
    // to get the precise classification per action.
    tool("memory", MU, ME),
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
    tool("str_replace", MU, A.union(FI)),
    tool("multi_edit", MU, A.union(FI)),
    tool("edit_file", MU, A.union(FI)),
    tool("apply_patch", MU, A.union(FI)),
    tool("create_file", MU, A.union(FI)),
    tool("delete_file", MU, A.union(FI)),
    tool("notebook_edit", MU, OR),
    // ── Mutating — rollback ──────────────────────────────────────────
    tool("rollback_file_edits", MU, A.union(OR)),
    tool("rollback_database_snapshots", MU, A.union(OR)),
    tool("rollback_session_state", MU, OR),
    // (memory/git/github entries consolidated above into action-aware rows)
    // ── Mutating — code intelligence writes ──────────────────────────
    tool("rename_symbol", MU, ToolFlags::CODE_INTEL),
    // ── Mutating — orchestration ─────────────────────────────────────
    tool("agent", MU, OR),
    tool("send_message", MU, OR),
    tool("share_context", MU, OR),
    tool("run_chain", MU, OR),
    tool("run_build_test", MU, OR),
    tool("config", MU, OR),
    tool("adjust_config", MU, OR),
    tool("compress_context", MU, OR),
    tool("env", MU, OR),
    // ── Mutating — task management (immune to deprioritization) ───────
    tool("task", MU, OR.union(ToolFlags::TASK_MGMT)),
    tool("task_output", RO, OR.union(ToolFlags::TASK_MGMT)),
    tool("task_list", RO, OR.union(ToolFlags::TASK_MGMT)),
    tool("task_stop", MU, OR.union(ToolFlags::TASK_MGMT)),
    // ── Shell execution (highest risk) ───────────────────────────────
    tool("bash", SH, AE.union(EX)),
    tool("exec", SH, AE.union(EX)),
    tool("run_command", SH, AE.union(EX)),
    tool("shell", SH, AE.union(EX)),
    tool("powershell", SH, AE.union(EX)),
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

    // ── Action-aware queries (for consolidated tools like `memory`) ──
    //
    // Most tools classify on name alone. A few consolidated tools
    // carry an `action` field whose value changes read/write semantics.
    // These helpers consult `args["action"]` and return the precise answer;
    // for name-only tools they fall back to the table row.

    pub fn idempotency_for(&self, name: &str, args: Option<&serde_json::Value>) -> ToolIdempotency {
        astra_turn_types::classify_tool_idempotency(name, args)
    }

    /// Returns the effective `ToolCategory` after inspecting `args` for
    /// consolidated tools.
    pub fn category_for(&self, name: &str, args: Option<&serde_json::Value>) -> ToolCategory {
        if matches!(name, "memory" | "git" | "github" | "task") {
            return classify(name, args).category;
        }
        self.category(name)
    }

    pub fn is_read_only_for(&self, name: &str, args: Option<&serde_json::Value>) -> bool {
        self.category_for(name, args).is_read_only()
    }

    pub fn is_mutating_for(&self, name: &str, args: Option<&serde_json::Value>) -> bool {
        self.category_for(name, args).is_mutating()
    }

    pub fn is_parallelizable_for(&self, name: &str, args: Option<&serde_json::Value>) -> bool {
        self.category_for(name, args).is_parallelizable()
    }

    // ── Derived queries (replace all hardcoded lists) ────────────────

    pub fn is_read_only(&self, name: &str) -> bool {
        self.category(name).is_read_only()
    }

    pub fn is_never_restrict(&self, name: &str) -> bool {
        self.category(name).is_never_restrict() || self.flags(name).contains(ToolFlags::TASK_MGMT)
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
        if name == "github" {
            return ToolDisplayCategory::Github;
        }
        // MCP-prefixed Memoria tools keep the Memory display slot even when
        // they don't land in TOOL_TABLE (e.g. dynamic plugin schemas).
        if name.starts_with("memoria_") {
            return ToolDisplayCategory::Memory;
        }
        let flags = self.flags(name);
        let category = self.category(name);
        if flags.contains(ToolFlags::CODE_INTEL) {
            ToolDisplayCategory::Code
        } else if flags.contains(ToolFlags::GIT_READ) || name == "git" {
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
                "search" | "grep" | "find" | "glob" | "list_dir" | "tool_search"
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
            .filter(|m| m.flags.contains(ToolFlags::APPROVAL_REQUIRED))
            .map(|m| m.name)
            .collect()
    }

    pub fn execute_command_names(&self) -> Vec<&'static str> {
        TOOL_TABLE
            .iter()
            .filter(|m| m.flags.contains(ToolFlags::EXECUTE_COMMAND))
            .map(|m| m.name)
            .collect()
    }

    pub fn shell_names(&self) -> Vec<&'static str> {
        TOOL_TABLE
            .iter()
            .filter(|m| m.category.is_shell())
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
        TOOL_TABLE.iter().map(|m| m.name).collect()
    }

    pub fn headless_read_only_names(&self) -> Vec<&'static str> {
        TOOL_TABLE
            .iter()
            .filter(|m| {
                m.category.is_read_only()
                    && !m.flags.contains(ToolFlags::WEB)
                    && !m.flags.contains(ToolFlags::MEMORY)
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

/// True when `name` is a dedicated file-mutation tool.
///
/// Derived from the [`ToolRegistry`]: a tool is a file-mutation tool when it is
/// registered as `Mutating` AND carries the `FILE_OP` flag, OR it is
/// `notebook_edit` (which carries `ORCHESTRATION` instead of `FILE_OP` but
/// mutates a file-like artifact and must never fall back to shell redirection).
///
/// When one of these tools has no edge execution in a turn, the
/// `no_matching_edge_execution_message` in `astra-turn-core::headless::assembly`
/// emits a transport-binding-failure message that forbids shell fallback
/// (bash/heredoc/python redirection) — those bypass file-edit guards.
///
/// Deriving from the registry (rather than a parallel hardcoded list) means
/// new file-mutation tools are covered automatically with no second list to
/// keep in sync.
#[must_use]
pub fn is_file_mutation_tool(name: &str) -> bool {
    let r = registry();
    // notebook_edit is ORCHESTRATION-flagged but mutates a file-like artifact;
    // treat it as a file mutation for transport-binding purposes.
    if name == "notebook_edit" {
        return true;
    }
    r.category(name).is_mutating() && r.flags(name).contains(ToolFlags::FILE_OP)
}

// ── Args-aware classification ─────────────────────────────────────────
//
// the reference agent's killer feature: `bash "git status"` is safe to run in
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
    let mut meta_category = r.category(name);
    let mut meta_flags = r.flags(name);

    if name == "memory" {
        match args.and_then(|a| a.get("action")).and_then(|v| v.as_str()) {
            Some("recall" | "expand" | "profile") => {
                meta_category = ToolCategory::ReadOnly;
                meta_flags = ME;
            }
            _ => {
                meta_category = ToolCategory::Mutating;
                meta_flags = ME;
            }
        }
    }

    if name == "task" {
        match args.and_then(|a| a.get("action")).and_then(|v| v.as_str()) {
            Some("list" | "get" | "list_user") => {
                meta_category = ToolCategory::ReadOnly;
                meta_flags = OR;
            }
            _ => {
                meta_category = ToolCategory::Mutating;
                meta_flags = OR;
            }
        }
    }

    if name == "git" {
        match args.and_then(|a| a.get("action")).and_then(|v| v.as_str()) {
            Some(
                "status" | "diff" | "log" | "show" | "blame" | "file_history" | "log_search"
                | "contributors",
            ) => {
                meta_category = ToolCategory::ReadOnly;
                meta_flags = GR;
            }
            Some("checkout_file" | "worktree") => {
                meta_category = ToolCategory::Mutating;
                meta_flags = NONE;
            }
            Some("commit" | "revert_commit" | "stash" | "push") | None => {
                meta_category = ToolCategory::Mutating;
                meta_flags = A;
            }
            Some(_) => {
                meta_category = ToolCategory::Mutating;
                meta_flags = A;
            }
        }
    }

    if name == "github" {
        match args.and_then(|a| a.get("action")).and_then(|v| v.as_str()) {
            Some(
                "list_prs" | "get_pr" | "ci_status" | "repo_stats" | "list_issues" | "get_issue",
            ) => {
                meta_category = ToolCategory::ReadOnly;
                meta_flags = GH;
            }
            Some("create_issue") | None => {
                meta_category = ToolCategory::Mutating;
                meta_flags = A;
            }
            Some(_) => {
                meta_category = ToolCategory::Mutating;
                meta_flags = A;
            }
        }
    }

    let shell_read_only = meta_category.is_shell()
        && args
            .and_then(|a| a.get("command"))
            .and_then(|v| v.as_str())
            .is_some_and(crate::cloud::approval_policy::bash_command_is_read_only);

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
    } else if name == "task" {
        match args.and_then(|a| a.get("action")).and_then(|v| v.as_str()) {
            Some("list" | "get" | "list_user") => ToolIdempotency::PureRead,
            _ => ToolIdempotency::NonIdempotent,
        }
    } else if matches!(name, "task_output" | "task_list") {
        ToolIdempotency::PureRead
    } else if name == "git" {
        match args.and_then(|a| a.get("action")).and_then(|v| v.as_str()) {
            Some(
                "status" | "diff" | "log" | "show" | "blame" | "file_history" | "log_search"
                | "contributors",
            ) => ToolIdempotency::PureRead,
            _ => ToolIdempotency::NonIdempotent,
        }
    } else if name == "github" {
        match args.and_then(|a| a.get("action")).and_then(|v| v.as_str()) {
            Some(
                "list_prs" | "get_pr" | "ci_status" | "repo_stats" | "list_issues" | "get_issue",
            ) => ToolIdempotency::PureRead,
            _ => ToolIdempotency::NonIdempotent,
        }
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

    const REMOVED_TOOL_NAMES: &[&str] = &[
        "file_read",
        "ReadFileTool",
        "Read",
        "View",
        "view",
        "ListDirTool",
        "GrepTool",
        "Grep",
        "GlobTool",
        "Glob",
        "get_file_contents",
        "search_code",
        "list_files",
        "find_files",
        "view_file",
        "open_file",
        "cat",
        "WebFetchTool",
        "WebSearchTool",
        "WriteFileTool",
        "Write",
        "EditFileTool",
        "Edit",
        "ApplyPatchTool",
        "BashTool",
        "Bash",
        "PowerShellTool",
    ];

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
    fn git_and_github_helper_style_names_are_not_registered() {
        let r = registry();

        for name in r.canonical_names() {
            assert!(
                !name.starts_with("git_") && !name.starts_with("github_"),
                "canonical tool names must use consolidated git/github action surfaces: {name}"
            );
        }

        let git_actions = [
            "status",
            "diff",
            "log",
            "show",
            "blame",
            "file_history",
            "contributors",
            "log_search",
            "checkout_file",
            "worktree",
        ];
        let github_actions = [
            "list_prs",
            "get_pr",
            "ci_status",
            "list_issues",
            "get_issue",
            "repo_stats",
        ];
        for name in git_actions
            .into_iter()
            .map(|action| format!("git_{action}"))
            .chain(
                github_actions
                    .into_iter()
                    .map(|action| format!("github_{action}")),
            )
        {
            assert!(r.get(&name).is_none(), "{name} should not be registered");
            assert_eq!(r.category(&name), ToolCategory::Mutating);
            assert_eq!(r.display_category(&name), ToolDisplayCategory::Other);
        }
    }

    #[test]
    fn consolidated_git_is_action_aware() {
        let status = classify("git", Some(&serde_json::json!({"action": "status"})));
        assert_eq!(status.category, ToolCategory::ReadOnly);
        assert!(!status.approval_required);
        assert!(status.parallelizable);
        assert_eq!(status.idempotency, ToolIdempotency::PureRead);

        let push = classify(
            "git",
            Some(&serde_json::json!({
                "action": "push",
                "remote": "origin",
                "branch": "feature/my-branch"
            })),
        );
        assert_eq!(push.category, ToolCategory::Mutating);
        assert!(push.approval_required);
        assert_eq!(push.idempotency, ToolIdempotency::NonIdempotent);
    }

    #[test]
    fn consolidated_github_is_action_aware() {
        let list = classify("github", Some(&serde_json::json!({"action": "list_prs"})));
        assert_eq!(list.category, ToolCategory::ReadOnly);
        assert!(!list.approval_required);
        assert!(list.parallelizable);
        assert_eq!(list.idempotency, ToolIdempotency::PureRead);

        let create = classify(
            "github",
            Some(&serde_json::json!({"action": "create_issue", "title": "bug"})),
        );
        assert_eq!(create.category, ToolCategory::Mutating);
        assert!(create.approval_required);
        assert_eq!(create.idempotency, ToolIdempotency::NonIdempotent);
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
    fn web_tools_are_read_only_and_compactable() {
        let r = registry();
        assert!(r.is_read_only("web_fetch"));
        assert!(r.is_compactable("web_fetch"));
        assert!(r.is_read_only("web_search"));
        assert!(r.is_compactable("web_search"));
    }

    #[test]
    fn memory_tool_is_conservative_by_name_and_action_aware_with_args() {
        use serde_json::json;
        let r = registry();
        // Name-only query: conservative Mutating (used when args aren't
        // available, e.g. static schema audits).
        assert_eq!(r.category("memory"), ToolCategory::Mutating);
        assert!(!r.is_read_only("memory"));
        assert!(!r.is_compactable("memory"));

        // Action-aware: recall/expand/profile are pure reads.
        for action in ["recall", "expand", "profile"] {
            let args = json!({"action": action});
            assert!(
                r.is_read_only_for("memory", Some(&args)),
                "memory(action={action}) should be read-only"
            );
            assert!(
                !r.is_mutating_for("memory", Some(&args)),
                "memory(action={action}) should NOT be mutating"
            );
        }

        // Mutating / side-effecting actions stay Mutating.
        for action in [
            "remember", "forget", "update", "focus", "reflect", "feedback",
        ] {
            let args = json!({"action": action});
            assert!(
                r.is_mutating_for("memory", Some(&args)),
                "memory(action={action}) should be mutating"
            );
            assert!(!r.is_read_only_for("memory", Some(&args)));
        }
    }

    #[test]
    fn consolidated_task_tool_is_action_aware_for_read_vs_mutating_actions() {
        use serde_json::json;

        for action in ["list", "get", "list_user"] {
            let read = classify("task", Some(&json!({"action": action})));
            assert_eq!(read.category, ToolCategory::ReadOnly);
            assert!(!read.approval_required);
            assert!(read.parallelizable);
        }

        let update = classify("task", Some(&json!({"action": "update"})));
        assert_eq!(update.category, ToolCategory::Mutating);
        assert!(!update.approval_required);

        let stale_background = classify(
            "task",
            Some(&json!({"action": "background_shell", "command": "npm run dev"})),
        );
        assert_eq!(stale_background.category, ToolCategory::Mutating);
        assert!(!stale_background.approval_required);
    }

    #[test]
    fn typed_background_task_tools_are_classified_by_operation() {
        let output = classify("task_output", None);
        assert_eq!(output.category, ToolCategory::ReadOnly);
        assert!(!output.approval_required);
        assert!(output.parallelizable);
        assert_eq!(output.idempotency, ToolIdempotency::PureRead);

        let list = classify("task_list", None);
        assert_eq!(list.category, ToolCategory::ReadOnly);
        assert_eq!(list.idempotency, ToolIdempotency::PureRead);

        let stop = classify("task_stop", None);
        assert_eq!(stop.category, ToolCategory::Mutating);
        assert!(!stop.approval_required);
        assert_eq!(stop.idempotency, ToolIdempotency::NonIdempotent);
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
    fn removed_tool_names_are_not_registered_and_fail_closed() {
        let r = registry();
        for name in REMOVED_TOOL_NAMES {
            assert!(r.get(name).is_none(), "{name} must not remain registered");

            let classification = classify_name(name);
            assert_eq!(
                classification.category,
                ToolCategory::Mutating,
                "{name} should use the unknown-tool category"
            );
            assert!(
                !classification.parallelizable,
                "{name} must not inherit read-only parallelism"
            );
            assert!(
                !classification.compactable,
                "{name} must not inherit compactability"
            );
            assert!(
                !classification.never_restrict,
                "{name} must not inherit never-restrict"
            );
            assert_eq!(
                classification.idempotency,
                ToolIdempotency::NonIdempotent,
                "{name} should use conservative retry semantics"
            );
            assert_eq!(
                r.display_category(name),
                ToolDisplayCategory::Other,
                "{name} should display as unknown/other"
            );
        }
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
    fn read_only_names_are_name_only_and_exclude_action_aware_tools() {
        let r = registry();
        let ro_names = r.read_only_names();
        for name in ["read_file", "list_dir", "grep", "glob"] {
            assert!(ro_names.contains(&name), "{name} should be read-only");
        }
        assert!(!ro_names.contains(&"git"));
        assert!(!ro_names.contains(&"github"));
    }

    #[test]
    fn compactable_names_are_name_only_and_exclude_action_aware_tools() {
        let r = registry();
        let compactable = r.compactable_names();
        for name in [
            "read_file",
            "grep",
            "glob",
            "list_dir",
            "web_search",
            "web_fetch",
            "symbols",
            "find_definition",
            "find_references",
        ] {
            assert!(compactable.contains(&name), "{name} should be compactable");
        }
        assert!(!compactable.contains(&"git"));
        assert!(!compactable.contains(&"github"));
    }

    #[test]
    fn headless_read_only_excludes_web_memory_and_dynamic_services() {
        let r = registry();
        let headless = r.headless_read_only_names();
        assert!(headless.contains(&"read_file"));
        assert!(headless.contains(&"symbols"));
        assert!(headless.contains(&"get_agent_info"));
        assert!(!headless.contains(&"git"));
        assert!(!headless.contains(&"github"));
        assert!(!headless.contains(&"web_fetch"));
        assert!(!headless.contains(&"web_search"));
        assert!(!headless.contains(&"memory"));
        for name in REMOVED_TOOL_NAMES {
            assert!(
                !headless.contains(name),
                "{name} must not be present in the headless read-only set"
            );
        }
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
    fn file_mutation_registry_includes_all_known_mutation_tools() {
        // Contract: any dedicated file-mutation tool that appears in the
        // registry as `Mutating + FILE_OP` (or notebook_edit, which carries
        // ORCHESTRATION) MUST be recognized by `is_file_mutation_tool`.
        // Otherwise its no-edge-execution message falls through to the
        // generic "use bash" branch, violating the no-shell-fallback guarantee.
        for meta in TOOL_TABLE {
            let is_file_mutating = meta.category.is_mutating()
                && (meta.flags.contains(ToolFlags::FILE_OP) || meta.name == "notebook_edit");
            if is_file_mutating {
                assert!(
                    is_file_mutation_tool(meta.name),
                    "{:?} is registered as Mutating + FILE_OP but is_file_mutation_tool returns false; \
                     add it so the no-edge-execution message never suggests shell fallback",
                    meta.name
                );
            }
        }
        // Sanity: the canonical mutation tools must always be recognized.
        for required in [
            "write_file",
            "str_replace",
            "multi_edit",
            "delete_file",
            "notebook_edit",
        ] {
            assert!(
                is_file_mutation_tool(required),
                "{required:?} must be recognized as a file mutation tool"
            );
        }
    }

    #[test]
    fn exploration_names_match_current_exploration_tools() {
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
    fn approval_required_tools_all_flagged() {
        let r = registry();
        for name in [
            "bash",
            "create_file",
            "delete_file",
            "edit_file",
            "exec",
            "git",
            "github",
            "multi_edit",
            "rollback_database_snapshots",
            "rollback_file_edits",
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
    fn mutating_git_github_helper_style_names_are_not_registered() {
        let r = registry();
        let git_actions = ["commit", "revert_commit", "stash"];
        let github_actions = ["create_issue"];
        for name in git_actions
            .into_iter()
            .map(|action| format!("git_{action}"))
            .chain(
                github_actions
                    .into_iter()
                    .map(|action| format!("github_{action}")),
            )
        {
            assert!(
                r.get(&name).is_none(),
                "{name} must not remain in TOOL_TABLE"
            );
        }
    }

    #[test]
    fn shell_execution_tools_all_flagged() {
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
    fn name_only_and_action_aware_read_tools_are_parallelizable() {
        let r = registry();
        for name in [
            "read_file",
            "grep",
            "glob",
            "list_dir",
            "web_fetch",
            "web_search",
            "find_definition",
            "find_references",
        ] {
            assert!(r.is_parallelizable(name), "{name} should be parallelizable");
        }

        use serde_json::json;
        for args in [
            json!({"action": "status"}),
            json!({"action": "diff"}),
            json!({"action": "log"}),
            json!({"action": "show"}),
            json!({"action": "blame"}),
            json!({"action": "file_history"}),
            json!({"action": "contributors"}),
            json!({"action": "log_search"}),
        ] {
            assert!(r.is_parallelizable_for("git", Some(&args)));
        }
        assert!(r.is_parallelizable_for("github", Some(&json!({"action": "list_prs"}))));
        assert!(r.is_parallelizable_for("github", Some(&json!({"action": "get_issue"}))));
        assert!(r.is_parallelizable_for("memory", Some(&json!({"action": "recall"}))));
        assert!(r.is_parallelizable_for("memory", Some(&json!({"action": "expand"}))));
        assert!(!r.is_parallelizable_for("git", Some(&json!({"action": "push"}))));
        assert!(!r.is_parallelizable_for("memory", Some(&json!({"action": "remember"}))));
        assert!(!r.is_parallelizable_for("memory", Some(&json!({"action": "forget"}))));
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
        let investigation = ["read_file", "grep", "find_definition", "symbols"];
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
        let diff_action = classify("git", Some(&serde_json::json!({"action": "diff"})));
        assert!(diff_action.parallelizable);
        assert!(diff_action.never_restrict);
        assert!(diff_action.compactable);
        assert!(!diff_action.approval_required);

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
        let verify = ["read_file"];
        for name in verify {
            assert!(r.is_parallelizable(name));
            assert!(r.is_never_restrict(name));
        }
        for args in [
            serde_json::json!({"action": "status"}),
            serde_json::json!({"action": "diff"}),
        ] {
            let classification = classify("git", Some(&args));
            assert!(classification.parallelizable);
            assert!(classification.never_restrict);
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
            ("bash", false),
            ("find_definition", true),
            ("delete_file", false),
        ];
        for (name, expect_parallel) in batch {
            assert_eq!(
                r.is_parallelizable(name),
                expect_parallel,
                "{name}: expected parallelizable={expect_parallel}"
            );
        }

        // `memory` must be partitioned by action, not by name.
        use serde_json::json;
        assert!(r.is_parallelizable_for("git", Some(&json!({"action": "status"}))));
        assert!(!r.is_parallelizable_for("git", Some(&json!({"action": "push"}))));
        assert!(r.is_parallelizable_for("memory", Some(&json!({"action": "recall"}))));
        assert!(!r.is_parallelizable_for("memory", Some(&json!({"action": "remember"}))));
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
    /// (no web, no memory), while cloud approval gates all
    /// mutating + shell tools. MCP tools (prefix mcp_) are unknown to the
    /// registry and get fail-closed defaults.
    #[test]
    fn scenario_cloud_headless_deployment() {
        let r = registry();
        let headless = r.headless_read_only_names();

        // Headless set includes core investigation tools
        assert!(headless.contains(&"read_file"));
        assert!(headless.contains(&"grep"));
        assert!(headless.contains(&"find_definition"));
        assert!(!headless.contains(&"git"));
        assert!(!headless.contains(&"github"));

        // Headless set excludes web (needs network), memory (needs server),
        // MatrixOne (needs DB), orchestration (agent-internal)
        assert!(!headless.contains(&"web_fetch"));
        assert!(!headless.contains(&"memory"));
        assert!(!headless.contains(&"mo_query"));
        assert!(!headless.contains(&"context_analysis"));

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
        for name in ["read_file", "grep", "glob", "find_definition"] {
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
        let status_action = serde_json::json!({"action": "status"});
        assert!(r.is_parallelizable_for("git", Some(&status_action)));
        assert!(crate::parallel_tool_exec::is_read_only_tool_with_args(
            "git",
            Some(&status_action)
        ));

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

    #[test]
    fn scenario_removed_names_do_not_receive_canonical_privileges() {
        let r = registry();
        for name in REMOVED_TOOL_NAMES {
            assert!(r.get(name).is_none(), "{name} should be removed");
            assert_eq!(
                r.category(name),
                ToolCategory::Mutating,
                "{name} should not inherit its former canonical category"
            );
            assert!(
                !r.is_parallelizable(name),
                "{name} should not inherit read-only parallelism"
            );
            assert!(
                !r.is_compactable(name),
                "{name} should not inherit compaction privileges"
            );
        }

        let c = classify(
            "BashTool",
            Some(&serde_json::json!({"command": "git status"})),
        );
        assert_eq!(c.category, ToolCategory::Mutating);
        assert!(!c.parallelizable);
        assert!(!c.compactable);
        assert_eq!(c.idempotency, ToolIdempotency::NonIdempotent);
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

    #[test]
    fn invariant_derived_name_lists_contain_only_registered_tools() {
        let r = registry();
        for (label, names) in [
            ("approval_required", r.approval_required_names()),
            ("execute_command", r.execute_command_names()),
            ("shell", r.shell_names()),
            ("canonical", r.canonical_names()),
        ] {
            for name in names {
                assert!(
                    r.get(name).is_some(),
                    "{label} list contains unregistered tool {name}"
                );
            }
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

    // ── Removed shell names do not get args-aware classification ─────

    #[test]
    fn classify_removed_bashtool_name_git_status_is_unknown_mutating() {
        let args = json!({"command": "git status"});
        let c = classify("BashTool", Some(&args));
        assert_eq!(c.category, ToolCategory::Mutating);
        assert!(!c.parallelizable);
        assert!(!c.approval_required);
        assert!(!c.compactable);
        assert_eq!(c.idempotency, ToolIdempotency::NonIdempotent);
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
            "web_fetch",
            "get_agent_info",
        ] {
            assert_eq!(
                r.idempotency(name),
                ToolIdempotency::PureRead,
                "{name} should be PureRead"
            );
        }
        use serde_json::json;
        for action in [
            "status",
            "log",
            "diff",
            "blame",
            "file_history",
            "contributors",
            "log_search",
        ] {
            assert_eq!(
                r.idempotency_for("git", Some(&json!({"action": action}))),
                ToolIdempotency::PureRead,
                "git(action={action}) should be PureRead"
            );
        }
        for action in [
            "list_prs",
            "get_pr",
            "list_issues",
            "get_issue",
            "ci_status",
            "repo_stats",
        ] {
            assert_eq!(
                r.idempotency_for("github", Some(&json!({"action": action}))),
                ToolIdempotency::PureRead,
                "github(action={action}) should be PureRead"
            );
        }
        for action in ["recall", "expand", "profile"] {
            assert_eq!(
                r.idempotency_for("memory", Some(&json!({"action": action}))),
                ToolIdempotency::PureRead,
                "memory(action={action}) should be PureRead"
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
            ToolIdempotency::NonIdempotent
        );
    }

    #[test]
    fn idempotency_mutating_tools_are_non_idempotent() {
        let r = registry();
        for name in [
            "bash",
            "str_replace",
            "delete_file",
            "multi_edit",
            "edit_file",
        ] {
            assert_eq!(
                r.idempotency(name),
                ToolIdempotency::NonIdempotent,
                "{name} should be NonIdempotent"
            );
        }
        assert_eq!(
            r.idempotency_for("git", Some(&serde_json::json!({"action": "commit"}))),
            ToolIdempotency::NonIdempotent
        );
        assert_eq!(
            r.idempotency_for(
                "github",
                Some(&serde_json::json!({"action": "create_issue"}))
            ),
            ToolIdempotency::NonIdempotent
        );
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

        let pure_reads = ["read_file", "grep"];
        for name in pure_reads {
            let idem = r.idempotency(name);
            assert!(
                idem.is_safe_to_retry(),
                "{name}: PureRead should be safe to retry"
            );
            assert!(idem.is_pure_read(), "{name}: should be pure read");
        }
        let status_action = r.idempotency_for("git", Some(&json!({"action": "status"})));
        assert!(status_action.is_safe_to_retry());
        assert!(status_action.is_pure_read());

        let idempotent_write = "write_file";
        let idem = r.idempotency(idempotent_write);
        assert!(
            idem.is_safe_to_retry(),
            "write_file: IdempotentWrite should be safe to retry"
        );
        assert!(!idem.is_pure_read(), "write_file: should NOT be pure read");

        let non_idempotent = ["bash", "str_replace"];
        for name in non_idempotent {
            let idem = r.idempotency(name);
            assert!(
                !idem.is_safe_to_retry(),
                "{name}: NonIdempotent should NOT be safe to retry"
            );
        }
        let github_create = r.idempotency_for("github", Some(&json!({"action": "create_issue"})));
        assert!(!github_create.is_safe_to_retry());
    }

    /// Full consistency: static and action-aware tools map to the correct
    /// retry/idempotency semantics through consolidated action surfaces.
    #[test]
    fn scenario_action_aware_idempotency_contract() {
        let r = registry();

        let pure_reads = [
            "read_file",
            "grep",
            "glob",
            "list_dir",
            "web_fetch",
            "get_agent_info",
        ];
        for name in pure_reads {
            assert_eq!(
                r.idempotency(name),
                ToolIdempotency::PureRead,
                "{name} should be PureRead"
            );
        }
        use serde_json::json;
        for action in [
            "status",
            "log",
            "diff",
            "blame",
            "file_history",
            "contributors",
            "log_search",
        ] {
            assert_eq!(
                r.idempotency_for("git", Some(&json!({"action": action}))),
                ToolIdempotency::PureRead,
                "git(action={action}) should be PureRead"
            );
        }
        for action in [
            "list_prs",
            "get_pr",
            "list_issues",
            "get_issue",
            "ci_status",
            "repo_stats",
        ] {
            assert_eq!(
                r.idempotency_for("github", Some(&json!({"action": action}))),
                ToolIdempotency::PureRead,
                "github(action={action}) should be PureRead"
            );
        }

        assert_eq!(
            r.idempotency("write_file"),
            ToolIdempotency::IdempotentWrite,
            "write_file should be IdempotentWrite"
        );

        let non_idempotent = ["bash", "str_replace"];
        for name in non_idempotent {
            assert_eq!(
                r.idempotency(name),
                ToolIdempotency::NonIdempotent,
                "{name} should be NonIdempotent"
            );
        }

        assert_eq!(
            r.idempotency_for("memory", Some(&json!({"action": "recall"}))),
            ToolIdempotency::PureRead,
        );
        assert_eq!(
            r.idempotency_for("memory", Some(&json!({"action": "remember"}))),
            ToolIdempotency::NonIdempotent,
        );
        assert_eq!(
            r.idempotency_for("github", Some(&json!({"action": "list_prs"}))),
            ToolIdempotency::PureRead,
        );
        assert_eq!(
            r.idempotency_for("github", Some(&json!({"action": "create_issue"}))),
            ToolIdempotency::NonIdempotent,
        );
    }

    /// Every tool in TOOL_TABLE must agree with the canonical
    /// classify_tool_idempotency function from astra-turn-types.
    #[test]
    fn invariant_table_idempotency_matches_shared_classifier() {
        for meta in TOOL_TABLE {
            assert_eq!(
                meta.idempotency,
                classify_tool_idempotency(meta.name, None),
                "TOOL_TABLE idempotency for {} disagrees with shared classify_tool_idempotency",
                meta.name,
            );
        }
    }

    // ── Display category tests ──────────────────────────────────────

    #[test]
    fn display_category_github_tools() {
        let r = registry();
        assert_eq!(r.display_category("github"), ToolDisplayCategory::Github);
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
        assert_eq!(r.display_category("git"), ToolDisplayCategory::Git);
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
        assert_eq!(r.display_category("mo_query"), ToolDisplayCategory::Mo);
    }

    #[test]
    fn display_category_memory_tool() {
        let r = registry();
        // All memory actions share the consolidated `memory` tool.
        assert_eq!(r.display_category("memory"), ToolDisplayCategory::Memory,);
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
            "share_context",
            "run_chain",
            "config",
            "adjust_config",
            "compress_context",
            "env",
            "notebook_edit",
            "task",
            "context_analysis",
            "diagnose",
            "rollback_file_edits",
            "rollback_database_snapshots",
            "rollback_session_state",
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

    #[test]
    fn removed_tool_names_display_as_other() {
        let r = registry();
        for name in REMOVED_TOOL_NAMES {
            assert_eq!(
                r.display_category(name),
                ToolDisplayCategory::Other,
                "{name} should not inherit a display category"
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
